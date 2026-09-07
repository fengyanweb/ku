use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fmt,
    rc::Rc,
};

use crate::{
    ast::{
        is_pure_append_argument, AssignTarget, BinaryOp, EnumDecl, Expr, ExprKind, Item, Literal,
        MatchArm, MatchPattern, ParamMode, Program, Stmt, StructDecl, TypeName, UnaryOp,
    },
    error::{KuError, KuResult},
    span::Span,
    stdlib::metadata::{self, ArgRule, Signature, TypePattern},
};

mod borrow;
mod monomorph;
/// Internal typed frame IR. This does not open the CLI's native async boundary.
pub mod task;
pub use borrow::verify_borrow_contract;

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
    pub mode: ParamMode,
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
    /// Cooperative native-handler cancellation branch. The backend only polls a
    /// thread-local deadline and chooses one edge; the explicit timeout edge is
    /// lowered through `return_terminator`, so every enclosing `finally` keeps
    /// its normal structured-control-flow semantics.
    Safepoint {
        continue_block: BlockId,
        timeout_block: BlockId,
    },
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
    /// A synchronous read of a non-owning parameter. This is never a move place.
    BorrowedParam(String),
    /// A shallow projected temporary whose owner remains outside this frame.
    BorrowedTemp(TempId),
    /// An argument passed to a View parameter. The operand remains caller-owned.
    Borrow(Box<IrExpr>),
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
    /// through a `__thunk`). Each captured cell records whether its pointer comes
    /// from a local cell variable or the current closure body's enclosing env;
    /// nested closures must not emit a nonexistent bare local for the latter.
    MakeClosure {
        function_id: FunctionId,
        captures: Vec<(String, IrType, IrCaptureSource)>,
    },
    /// Stage 6b: read a cell's payload (`cell->value`). The inner expr evaluates
    /// to a `KuCell*`; the result type is the payload type.
    CellLoad(Box<IrExpr>),
    /// Stage 6b: inside a closure body, the captured cell pointer for `name`
    /// (resolves to `__e->{name}`). Its type is `Cell(payload)`.
    CapturedCell(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrCaptureSource {
    Local,
    EnclosingEnvironment,
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
        param_modes: Vec<ParamMode>,
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
    param_modes: Vec<ParamMode>,
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

/// A fresh root created while evaluating this call must outlive its borrowed
/// projection, but not an argument evaluation that exits through `?`.
fn borrow_temporary_owner(expr: &IrExpr, first_argument_temp: usize) -> Option<&IrExpr> {
    let mut owner = expr;
    while let IrExprKind::Field { target, .. } | IrExprKind::Index { target, .. } = &owner.kind {
        owner = target;
    }
    match owner.kind {
        IrExprKind::Temp(id) if id.0 >= first_argument_temp && ir_type_is_owned(&owner.ty) => {
            Some(owner)
        }
        _ => None,
    }
}

/// Read-only provenance follows transparent projections, never an owning call
/// result (including an explicit clone).
pub(crate) fn ir_expr_is_borrowed(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::BorrowedParam(_) | IrExprKind::BorrowedTemp(_) | IrExprKind::Borrow(_) => true,
        IrExprKind::Field { target, .. } | IrExprKind::Index { target, .. } => {
            ir_expr_is_borrowed(target)
        }
        _ => false,
    }
}

/// Whether a value of this type owns heap memory and therefore needs a clone/drop
/// (rather than being a trivially-copyable Copy value). Matches the set of types
/// `c_clone_expr` / `c_drop_value` know how to handle in the backend.
fn ir_type_is_owned(ty: &IrType) -> bool {
    match ty {
        IrType::Str | IrType::Array(_) | IrType::Result(_) | IrType::Closure { .. } => true,
        // The native Time ABI is a tagged, by-value pair. It is intentionally
        // distinct from a dynamic object and carries no heap ownership.
        IrType::Named(name) => name != TIME_TYPE,
        _ => false,
    }
}

pub fn lower_program(program: &Program) -> KuResult<IrProgram> {
    crate::ast::reject_compiled_async(
        program,
        "async/await is not supported by IR/native lowering yet",
    )?;
    let specialized = monomorph::specialize(program)?;
    let program = specialized.as_ref().unwrap_or(program);
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
                    param_modes: function.params.iter().map(|p| p.mode).collect(),
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
            let throwaway_lifted: Rc<RefCell<Vec<IrFunction>>> = Rc::new(RefCell::new(Vec::new()));
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
                .lower_block_body("entry", &function.body, function.span, &function.params)
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
                    mode: param.mode,
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
            lower.lower_block_body("entry", &function.body, function.span, &function.params)?;
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
    let program = IrProgram { functions, layouts };
    verify_borrow_contract(&program)?;
    Ok(program)
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
        | IrTerminator::Safepoint { .. }
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
        IrExprKind::Borrow(inner) => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::Borrow(Box::new(optimize_expr(*inner))),
        },
        IrExprKind::Literal(_)
        | IrExprKind::BorrowedParam(_)
        | IrExprKind::BorrowedTemp(_)
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
        }
        | IrTerminator::Safepoint {
            continue_block: body_block,
            timeout_block: after_block,
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
                write!(
                    f,
                    "{}{}: {}",
                    if param.mode == ParamMode::View {
                        "&"
                    } else {
                        ""
                    },
                    param.name,
                    param.ty
                )?;
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
            IrTerminator::Safepoint {
                continue_block,
                timeout_block,
            } => write!(
                f,
                "safepoint continue block{} timeout block{}",
                continue_block.0, timeout_block.0
            ),
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
            IrExprKind::BorrowedParam(name) => write!(f, "borrowed {name}"),
            IrExprKind::BorrowedTemp(id) => write!(f, "borrowed %t{}", id.0),
            IrExprKind::Borrow(inner) => write!(f, "borrow({inner})"),
            IrExprKind::CapturedCell(name) => write!(f, "captured_cell {name}"),
        }
    }
}

struct FunctionLowerer<'a> {
    signatures: &'a HashMap<String, FunctionSig>,
    layouts: &'a IrLayoutTable,
    return_type: IrType,
    locals: HashMap<String, IrType>,
    borrowed_params: HashSet<String>,
    /// Source spellings resolve to unique C/IR names while a lexical binding is
    /// visible. The type table keeps emitted bindings for place/cleanup typing.
    local_names: HashMap<String, String>,
    /// Undo only aliases introduced by a lexical scope, rather than copying the
    /// whole function's local table at every block.
    local_scopes: Vec<Vec<(String, Option<String>)>>,
    blocks: Vec<IrBlock>,
    current: IrBlock,
    next_block_id: usize,
    next_temp_id: usize,
    try_handlers: Vec<IrTryHandler>,
    pending_borrow_temporaries: Vec<PendingBorrowTemporary>,
    pattern_bindings: HashMap<String, IrExpr>,
    /// Program-global FunctionId allocator, shared between the top-level lowerer
    /// and every child lowerer that lifts a closure body (Stage 6a).
    next_function_id: Rc<Cell<usize>>,
    /// Closure bodies lifted out of expressions, appended to the program's
    /// functions once every top-level function has been lowered.
    lifted_functions: Rc<RefCell<Vec<IrFunction>>>,
    /// Stage 6b: binding sites declared in this function body that a closure
    /// created while that exact binding is visible captures. Keeping the source
    /// binding site (rather than only its spelling) prevents a closure-local or
    /// later same-named binding from boxing an unrelated local.
    boxed: HashSet<BoxedBindingSite>,
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
    self_param_modes: Vec<ParamMode>,
}

/// The source definition that owns a local binding. Statement offsets are
/// stable within one parsed program, and `name` disambiguates the multiple
/// bindings introduced by a destructuring assignment at the same statement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BoxedBindingSite {
    name: String,
    start_offset: usize,
    end_offset: usize,
}

impl BoxedBindingSite {
    fn new(name: &str, span: Span) -> Self {
        Self {
            name: name.to_string(),
            start_offset: span.start.offset,
            end_offset: span.end.offset,
        }
    }
}

/// The parser has separate parameter node types for declarations and closure
/// literals. Lowering only needs their shared binding identity, so use a small
/// borrowed adapter instead of allocating a parallel parameter-site vector.
trait BodyParameter {
    fn binding_name(&self) -> &str;
    fn binding_span(&self) -> Span;
    fn mode(&self) -> ParamMode;
}

impl BodyParameter for crate::ast::Param {
    fn mode(&self) -> ParamMode {
        self.mode
    }
    fn binding_name(&self) -> &str {
        &self.name
    }

    fn binding_span(&self) -> Span {
        self.span
    }
}

impl BodyParameter for crate::ast::FunctionParam {
    fn mode(&self) -> ParamMode {
        self.mode
    }
    fn binding_name(&self) -> &str {
        &self.name
    }

    fn binding_span(&self) -> Span {
        self.span
    }
}

#[derive(Debug, Clone)]
struct IrTryHandler {
    error_block: BlockId,
    error_name: String,
    return_block: Option<BlockId>,
    return_name: Option<String>,
}

struct PendingBorrowTemporary {
    owner: IrExpr,
    /// Only an error reaching the handler outside this argument evaluation
    /// aborts it. A nested handler that catches its own error must not drop it.
    error_block: Option<BlockId>,
}

struct LoweredCaptureBindings {
    values: Vec<(String, IrType, IrCaptureSource)>,
    aliases: HashMap<String, String>,
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
            borrowed_params: HashSet::new(),
            local_names: HashMap::new(),
            local_scopes: Vec::new(),
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
            pending_borrow_temporaries: Vec::new(),
            pattern_bindings: HashMap::new(),
            next_function_id,
            lifted_functions,
            boxed: HashSet::new(),
            captures: HashMap::new(),
            self_recurse: None,
            self_param_modes: Vec::new(),
        }
    }

    fn lower_block_body<P: BodyParameter>(
        &mut self,
        name: &str,
        body: &[Stmt],
        span: Span,
        parameters: &[P],
    ) -> KuResult<()> {
        // Stage 6b: any local this body declares that a nested closure literal
        // captures must be boxed into a shared `KuCell` at its declaration. A
        // captured parameter is owned by this function too, so box it once at
        // entry; enclosing captures only shadow assignment-created locals.
        let lexical_bindings = self
            .locals
            .keys()
            .chain(self.captures.keys())
            .chain(self.local_names.keys())
            .cloned()
            .collect::<HashSet<_>>();
        self.boxed = collect_boxed_candidates(body, &lexical_bindings, parameters);
        self.borrowed_params = parameters
            .iter()
            .filter(|p| p.mode() == ParamMode::View)
            .map(|p| p.binding_name().to_owned())
            .collect();
        for parameter in parameters {
            if parameter.mode() == ParamMode::View
                && self.boxed.contains(&BoxedBindingSite::new(
                    parameter.binding_name(),
                    parameter.binding_span(),
                ))
            {
                return Err(KuError::runtime(
                    "cannot capture borrowed parameter",
                    parameter.binding_span(),
                ));
            }
        }
        self.current.name = name.to_string();
        for parameter in parameters {
            let parameter_name = parameter.binding_name();
            let parameter_span = parameter.binding_span();
            if !self
                .boxed
                .contains(&BoxedBindingSite::new(parameter_name, parameter_span))
            {
                continue;
            }
            let ty = self.locals.get(parameter_name).cloned().ok_or_else(|| {
                KuError::runtime(
                    format!("missing IR type for captured parameter '{parameter_name}'"),
                    parameter_span,
                )
            })?;
            self.push_cell_new(
                parameter_name.to_string(),
                ty.clone(),
                IrExpr {
                    kind: IrExprKind::Local(parameter_name.to_string()),
                    ty,
                },
                parameter_span,
            )?;
        }
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

    fn local_ir_name<'b>(&'b self, name: &'b str) -> &'b str {
        self.local_names
            .get(name)
            .map(String::as_str)
            .unwrap_or(name)
    }

    fn define_local(&mut self, name: &str, ty: IrType) -> String {
        let visible_name = self.local_ir_name(name);
        let needs_unique_name = !self.local_scopes.is_empty()
            || self.locals.contains_key(visible_name)
            || self.captures.contains_key(visible_name);
        let ir_name = if needs_unique_name {
            loop {
                let candidate = format!("__ku_local_{}_{}", self.next_temp_id, name);
                self.next_temp_id += 1;
                if !self.locals.contains_key(&candidate) && !self.captures.contains_key(&candidate)
                {
                    break candidate;
                }
            }
        } else {
            name.to_string()
        };
        if let Some(scope) = self.local_scopes.last_mut() {
            scope.push((name.to_string(), self.local_names.get(name).cloned()));
        }
        if ir_name != name {
            self.local_names.insert(name.to_string(), ir_name.clone());
        }
        self.locals.insert(ir_name.clone(), ty);
        ir_name
    }

    fn with_local_scope<T>(&mut self, lower: impl FnOnce(&mut Self) -> KuResult<T>) -> KuResult<T> {
        self.local_scopes.push(Vec::new());
        let result = lower(self);
        for (name, previous) in self
            .local_scopes
            .pop()
            .expect("IR lexical scope was pushed")
            .into_iter()
            .rev()
        {
            if let Some(previous) = previous {
                self.local_names.insert(name, previous);
            } else {
                self.local_names.remove(&name);
            }
        }
        result
    }

    /// Overlay one match arm's bindings while preserving any bindings from an
    /// enclosing match expression. The undo log is proportional only to the
    /// inner pattern, avoiding a clone of the whole outer overlay for every arm.
    fn push_pattern_bindings(
        &mut self,
        bindings: HashMap<String, IrExpr>,
    ) -> Vec<(String, Option<IrExpr>)> {
        let mut undo = Vec::with_capacity(bindings.len());
        for (name, value) in bindings {
            let previous = self.pattern_bindings.insert(name.clone(), value);
            undo.push((name, previous));
        }
        undo
    }

    fn pop_pattern_bindings(&mut self, undo: Vec<(String, Option<IrExpr>)>) {
        for (name, previous) in undo.into_iter().rev() {
            if let Some(previous) = previous {
                self.pattern_bindings.insert(name, previous);
            } else {
                self.pattern_bindings.remove(&name);
            }
        }
    }

    fn lower_statements(&mut self, body: &[Stmt]) -> KuResult<()> {
        for stmt in body {
            self.lower_stmt(stmt)?;
            if self.current.terminator != IrTerminator::Next {
                break;
            }
        }
        Ok(())
    }

    fn lower_scoped_statements(&mut self, body: &[Stmt]) -> KuResult<()> {
        self.with_local_scope(|lower| lower.lower_statements(body))
    }

    fn lower_stmt(&mut self, stmt: &Stmt) -> KuResult<()> {
        match stmt {
            Stmt::VarDecl {
                name,
                ty,
                value,
                span: stmt_span,
                ..
            } => {
                let span = value.span;
                let declared = ty.as_ref().map(|ty| lower_type(ty, self.layouts));
                let value = self.lower_expr_with_expected(value, declared.as_ref())?;
                let ty = declared.unwrap_or_else(|| value.ty.clone());
                if self.binding_is_boxed(name, *stmt_span) {
                    self.push_cell_new(name.clone(), ty, value, span)?;
                } else {
                    let name = self.define_local(name, ty.clone());
                    self.current
                        .instructions
                        .push(IrInst::Let { name, ty, value });
                }
            }
            Stmt::Assign {
                name,
                value,
                span: stmt_span,
            } => {
                let span = value.span;
                // A closure assigned to an already-declared function-typed local
                // takes that local's type as its expected function type.
                let ir_name = self.local_ir_name(name);
                let expected = match self.locals.get(ir_name) {
                    Some(IrType::Cell(inner)) => Some((**inner).clone()),
                    Some(ty) => Some(ty.clone()),
                    None => self.captures.get(ir_name).map(|ty| match ty {
                        IrType::Cell(inner) => (**inner).clone(),
                        other => other.clone(),
                    }),
                };
                let value = if let Some(value) = self.lower_local_array_append(name, value)? {
                    value
                } else {
                    self.lower_expr_with_expected(value, expected.as_ref())?
                };
                let define_boxed = self.binding_is_boxed(name, *stmt_span);
                self.store_or_define_name(name, value, span, define_boxed)?;
            }
            Stmt::AssignTarget { target, value, .. } => {
                let value = self.lower_expr(value)?;
                // Match the interpreter: evaluate the RHS completely before any
                // index expression in the destination. Materializing here also
                // prevents a later target-side effect from changing a local/field
                // that the RHS merely referenced.
                let value = self.emit_temp(value)?;
                let target = self.lower_lvalue(target)?;
                self.current
                    .instructions
                    .push(IrInst::Store { target, value });
            }
            Stmt::CompoundAssign {
                target, op, value, ..
            } => {
                // Compound assignment follows the same RHS-first rule as the
                // interpreter. Keep it in a temp while the destination place and
                // its indexes are resolved exactly once.
                let right = self.lower_expr(value)?;
                let right = self.emit_temp(right)?;
                if let AssignTarget::Variable(name) = target {
                    if *op == BinaryOp::Add
                        && right.ty == IrType::Str
                        && self.locals.get(self.local_ir_name(name)) == Some(&IrType::Str)
                        && self.assignment_cell(name).is_none()
                    {
                        let value = self.emit_local_collection_reuse(
                            "__ku_string_concat_reuse",
                            name,
                            IrType::Str,
                            right,
                        )?;
                        self.current.instructions.push(IrInst::Store {
                            target: IrLValue::Local(self.local_ir_name(name).to_string()),
                            value,
                        });
                        return Ok(());
                    }
                    if let Some(cell) = self.assignment_cell(name) {
                        let inner = match &cell.ty {
                            IrType::Cell(inner) => (**inner).clone(),
                            _ => IrType::Unknown,
                        };
                        let left = IrExpr {
                            kind: IrExprKind::CellLoad(Box::new(cell.clone())),
                            ty: inner,
                        };
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
                            .push(IrInst::CellStore { cell, value });
                        return Ok(());
                    }
                }
                let target = self.lower_lvalue_cached(target)?;
                let left = self.lvalue_read_expr(&target);
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
                    let define_boxed = self.binding_is_boxed(name, *span);
                    self.store_or_define_name(name, value, *span, define_boxed)?;
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
                span,
            } => {
                let iterable = self.lower_expr(iterable)?;
                self.lower_for(name, iterable, body, *span)?;
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
                                kind: IrExprKind::Local("fail".to_string()),
                                ty: IrType::Function,
                            }),
                            args: vec![value],
                            kind: IrCallKind::Intrinsic("fail".to_string()),
                        },
                        ty: result_ty,
                    };
                    // The catch slot takes this Result's error before JumpErr
                    // reads it again. Keep one owner, not a constructor rvalue
                    // that would move the same payload twice.
                    let result = self.emit_temp(result)?;
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
                let needs_safepoint = ir_expr_needs_post_call_safepoint(&value);
                self.current.instructions.push(IrInst::Expr(value));
                // A void-returning call is not materialized by `emit_temp`, so
                // statement position appends its post-call cancellation edge.
                if needs_safepoint {
                    self.emit_safepoint();
                }
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
        self.lower_scoped_statements(then_branch)?;
        if self.current.terminator == IrTerminator::Next {
            self.current.terminator = IrTerminator::Jump(after_id);
        }
        self.finish_current();

        self.start_block(else_id, "else");
        self.lower_scoped_statements(else_branch)?;
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
        self.lower_scoped_statements(body)?;
        if self.current.terminator == IrTerminator::Next {
            // A tight compute loop remains cancellable even when its body has no
            // calls. `emit_safepoint` creates an explicit timeout return edge, so
            // an enclosing try/finally sees the same structured return route.
            self.emit_safepoint();
            self.current.terminator = IrTerminator::Jump(cond_id);
        }
        self.finish_current();

        self.start_block(after_id, "while_after");
        Ok(())
    }

    fn lower_for(
        &mut self,
        name: &str,
        iterable: IrExpr,
        body: &[Stmt],
        span: Span,
    ) -> KuResult<()> {
        let element = match &iterable.ty {
            IrType::Array(element) => (**element).clone(),
            IrType::Int => IrType::Int,
            IrType::Unknown => IrType::Unknown,
            other => {
                return Err(KuError::runtime(
                    format!("IR/native for expects array or int but got {other}"),
                    span,
                ));
            }
        };
        // A loop iterator is a fresh lexical binding on every interpreter
        // iteration. The current native closure ABI boxes declaration sites,
        // while `ForEach` binds its value in a terminator, so silently treating
        // this as an ordinary local would either miss the capture or make all
        // iterations share the wrong cell. Reject this one unsupported corner
        // until the IR can model per-iteration cells explicitly.
        if self.binding_is_boxed(name, span) {
            return Err(KuError::runtime(
                "IR/native lowering does not support closure capture of a for loop variable yet",
                span,
            ));
        }

        self.with_local_scope(|lower| lower.lower_for_body(name, element, iterable, body))
    }

    fn lower_for_body(
        &mut self,
        name: &str,
        element: IrType,
        iterable: IrExpr,
        body: &[Stmt],
    ) -> KuResult<()> {
        // `iterable` has already been lowered into the current block. Put the
        // actual iterator terminator in a fresh header block: the loop backedge
        // must not re-run calls/array construction used to compute the iterable.
        let iter_id = self.next_block("for_iter");
        let body_id = self.next_block("for_body");
        let after_id = self.next_block("for_after");
        self.current.terminator = IrTerminator::Jump(iter_id);
        self.finish_current();

        self.start_block(iter_id, "for_iter");
        let ir_name = self.define_local(name, element);
        self.current.terminator = IrTerminator::ForEach {
            name: ir_name,
            iterable,
            body_block: body_id,
            after_block: after_id,
        };
        self.finish_current();

        self.start_block(body_id, "for_body");
        self.lower_statements(body)?;
        if self.current.terminator == IrTerminator::Next {
            // `for` can iterate up to the full i64/array range. Poll on every
            // back-edge just like `while`, otherwise a handler with a call-free
            // loop can ignore its cooperative deadline indefinitely.
            self.emit_safepoint();
            self.current.terminator = IrTerminator::Jump(iter_id);
        }
        self.finish_current();

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
        let body_result = self.lower_scoped_statements(body);
        self.try_handlers.pop();
        body_result?;
        if self.current.terminator == IrTerminator::Next {
            self.current.instructions.push(IrInst::EndTry);
            self.current.terminator = IrTerminator::Jump(finally_id.unwrap_or(after_id));
        }
        self.finish_current();

        if let Some(catch_id) = catch_id {
            self.start_block(catch_id, "catch");
            self.with_local_scope(|lower| {
                if let Some(name) = catch_name {
                    let name = lower.define_local(name, error_ir_type());
                    lower.current.instructions.push(IrInst::BindError {
                        name,
                        result: IrExpr {
                            kind: IrExprKind::Local(error_name.clone()),
                            ty: lower
                                .locals
                                .get(&error_name)
                                .cloned()
                                .unwrap_or_else(|| IrType::Result(Box::new(IrType::Null))),
                        },
                    });
                }
                if let Some(finally_err_id) = finally_err_id {
                    lower.try_handlers.push(IrTryHandler {
                        error_block: finally_err_id,
                        error_name: error_name.clone(),
                        return_block: finally_return_id,
                        return_name: return_name.clone(),
                    });
                }
                let result = lower.lower_statements(catch_body);
                if finally_err_id.is_some() {
                    lower.try_handlers.pop();
                }
                result
            })?;
            if self.current.terminator == IrTerminator::Next {
                self.current.terminator = IrTerminator::Jump(finally_id.unwrap_or(after_id));
            }
            self.finish_current();
        }

        if let Some(finally_id) = finally_id {
            self.start_block(finally_id, "finally");
            self.lower_scoped_statements(finally_body)?;
            if self.current.terminator == IrTerminator::Next {
                self.current.terminator = IrTerminator::Jump(after_id);
            }
            self.finish_current();
        }

        if let Some(finally_err_id) = finally_err_id {
            self.start_block(finally_err_id, "finally_err");
            self.lower_scoped_statements(finally_body)?;
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
            self.lower_scoped_statements(finally_body)?;
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
            AssignTarget::Variable(name) => {
                Ok(IrLValue::Local(self.local_ir_name(name).to_string()))
            }
            AssignTarget::Index { target, index } => Ok(IrLValue::Index {
                // A field-held array is a writable place, not a value read.
                // lower_expr would clone its field and silently store into that
                // temporary instead of updating the containing struct.
                target: self.lower_lvalue_target_expr(target)?,
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
            AssignTarget::Variable(name) => {
                Ok(IrLValue::Local(self.local_ir_name(name).to_string()))
            }
            AssignTarget::Index { target, index } => {
                // Preserve the container as a place. Materializing an owned array
                // local here would move it into a temp and clear the actual
                // assignment root. Only index expressions need caching.
                let target = self.lower_lvalue_target_expr_cached(target)?;
                let index = self.lower_expr(index)?;
                let index = self.emit_temp(index)?;
                Ok(IrLValue::Index { target, index })
            }
            AssignTarget::Field { target, name } => Ok(IrLValue::Field {
                target: self.lower_lvalue_target_expr_cached(target)?,
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
                return Err(KuError::runtime(
                    "unterminated template interpolation",
                    span,
                ));
            }
            if source.trim().is_empty() {
                return Err(KuError::runtime("empty template interpolation", span));
            }
            let tokens = crate::lexer::Lexer::new(&source).tokenize()?;
            let expr = crate::parser::Parser::new(tokens).parse_expression_only()?;
            crate::ast::reject_compiled_async_expression(
                &expr,
                "async/await is not supported by IR/native lowering yet",
            )?;
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
        let mut acc = iter
            .next()
            .unwrap_or_else(|| Expr::new(ExprKind::Literal(Literal::String(String::new())), span));
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

    /// The readable type of a literal or plain place, without emitting anything.
    /// Cells expose their payload type here; taking a snapshot is a separate step.
    fn static_place_type(&self, expr: &Expr) -> Option<IrType> {
        match &expr.kind {
            ExprKind::Literal(literal) => Some(literal_type(literal)),
            ExprKind::Variable(name) => self
                .pattern_bindings
                .get(name)
                .map(|value| value.ty.clone())
                .or_else(|| {
                    let name = self.local_ir_name(name);
                    self.locals
                        .get(name)
                        .or_else(|| self.captures.get(name))
                        .cloned()
                })
                .map(|ty| match ty {
                    IrType::Cell(inner) => *inner,
                    other => other,
                }),
            ExprKind::Field { target, name } => {
                let target_ty = self.static_place_type(target)?;
                Some(self.field_type(&target_ty, name))
            }
            ExprKind::Index { target, .. } => match self.static_place_type(target)? {
                IrType::Array(element) => Some(*element),
                _ => None,
            },
            _ => None,
        }
    }

    /// Inspect a prospective method receiver without emitting its effects. This
    /// intentionally does not turn arbitrary fields into callable values. The
    /// caller only uses a proven Str to recognize temporary string receivers;
    /// their real evaluation still goes through the ordinary owned-temp path.
    fn static_receiver_type(&self, expr: &Expr, depth: usize) -> Option<IrType> {
        if depth >= 64 {
            return None;
        }
        if let Some(ty) = self.static_place_type(expr) {
            return Some(ty);
        }
        match &expr.kind {
            ExprKind::TryUnwrap { expr } => match self.static_receiver_type(expr, depth + 1)? {
                IrType::Result(inner) => Some(*inner),
                _ => None,
            },
            ExprKind::Call { callee, .. } => {
                if let ExprKind::Variable(_) = &callee.kind {
                    // A local function value shadows an identically named
                    // top-level function or builtin, including its return type.
                    if let Some(bound) = self.static_place_type(callee) {
                        return match bound {
                            IrType::Closure { ret, .. } => Some(*ret),
                            _ => None,
                        };
                    }
                }
                if let ExprKind::Field { target, name } = &callee.kind {
                    if let Some(receiver) = self.static_receiver_type(target, depth + 1) {
                        if name == "clone" {
                            return Some(receiver);
                        }
                        let signature = match &receiver {
                            IrType::Str => metadata::dotted_signature("string", name),
                            IrType::Named(native)
                                if native == metadata::MYSQL_CLIENT
                                    || native == metadata::MYSQL_RESULT =>
                            {
                                metadata::mysql_method_signature(native, name)
                            }
                            IrType::Named(native) if native == "__ku_pg_result" => {
                                metadata::dotted_signature("pg_result", name)
                            }
                            IrType::Named(native) if native == "__ku_value" => {
                                metadata::dotted_signature("kuvalue", name)
                            }
                            _ => None,
                        };
                        return signature.map(|signature| signature_return_type(&signature, &[]));
                    }
                }
                let (_, ty) = call_kind_and_type(callee, &[], self.signatures);
                (ty != IrType::Unknown).then_some(ty)
            }
            ExprKind::Binary { left, op, right } if *op == BinaryOp::Add => {
                let left = self.static_receiver_type(left, depth + 1)?;
                let right = self.static_receiver_type(right, depth + 1)?;
                (left == IrType::Str && right == IrType::Str).then_some(IrType::Str)
            }
            _ => None,
        }
    }

    fn lower_lvalue_target_expr(&mut self, expr: &Expr) -> KuResult<IrExpr> {
        match &expr.kind {
            ExprKind::Variable(_) => self.lower_expr(expr),
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
            ExprKind::Index { target, index } => {
                // Keep an indexed projection as a place instead of lowering it to
                // a value copy. The C backend can then address the real array slot
                // for targets such as `values[i].field` and nested indexes.
                let target = self.lower_lvalue_target_expr(target)?;
                let index = self.lower_expr(index)?;
                let ty = match &target.ty {
                    IrType::Array(element) => *element.clone(),
                    _ => IrType::Unknown,
                };
                Ok(IrExpr {
                    kind: IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    ty,
                })
            }
            _ => self.lower_expr(expr),
        }
    }

    /// Cached variant used by compound assignment. It preserves every container
    /// projection as an addressable place while materializing each user-supplied
    /// index exactly once. In particular, it never moves an owned array root into
    /// a temporary merely to cache its header.
    fn lower_lvalue_target_expr_cached(&mut self, expr: &Expr) -> KuResult<IrExpr> {
        match &expr.kind {
            ExprKind::Variable(_) => self.lower_expr(expr),
            ExprKind::Field { target, name } => {
                let target = self.lower_lvalue_target_expr_cached(target)?;
                let ty = self.field_type(&target.ty, name);
                Ok(IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(target),
                        name: name.clone(),
                    },
                    ty,
                })
            }
            ExprKind::Index { target, index } => {
                let target = self.lower_lvalue_target_expr_cached(target)?;
                let index = self.lower_expr(index)?;
                let index = self.emit_temp(index)?;
                let ty = match &target.ty {
                    IrType::Array(element) => *element.clone(),
                    _ => IrType::Unknown,
                };
                Ok(IrExpr {
                    kind: IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
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
    /// String messages remain strings here so every backend can represent them;
    /// direct/try lowering tells its backend that this is `fail`, not `err`.
    /// An existing Error keeps its original domain and code.
    fn lower_error_expr(&mut self, value: &Expr) -> KuResult<IrExpr> {
        if let ExprKind::ObjectLiteral { fields } = &value.kind {
            let mut domain = None;
            let mut code = None;
            let mut message = None;
            for (index, (name, _)) in fields.iter().enumerate() {
                match name.as_str() {
                    "domain" => domain = Some(index),
                    "code" => code = Some(index),
                    "message" => message = Some(index),
                    _ => {}
                }
            }
            if let (Some(domain), Some(code), Some(message)) = (domain, code, message) {
                if fields.len() == 3 {
                    // Object fields evaluate in source order. Freeze each value
                    // before later callbacks, then arrange the already-owned
                    // temps for the fixed domain/code/message ABI.
                    let mut values = Vec::with_capacity(fields.len());
                    for (_, value) in fields {
                        let value = self.lower_expr(value)?;
                        values.push(self.emit_temp(value)?);
                    }
                    return Ok(IrExpr {
                        kind: IrExprKind::Call {
                            callee: Box::new(IrExpr {
                                kind: IrExprKind::Local("__ku_error_make".to_string()),
                                ty: IrType::Function,
                            }),
                            args: vec![
                                values[domain].clone(),
                                values[code].clone(),
                                values[message].clone(),
                            ],
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
        if method == "bind" {
            return Err(KuError::runtime(
                "http service bind/listener run/close are interpreter-only; the native C backend supports app.listen(address)",
                span,
            ));
        }
        if method == "listen" {
            if args.len() != 1 {
                return Err(KuError::runtime(
                    format!(
                        "http service listen expects 1 argument but got {}",
                        args.len()
                    ),
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
                format!(
                    "http service {method} expects 2 arguments but got {}",
                    args.len()
                ),
                span,
            ));
        }
        let path = self.lower_expr(&args[0])?;
        let expected = IrType::Closure {
            params: vec![IrType::Named(HTTP_REQUEST_TYPE.to_string())],
            param_modes: vec![ParamMode::Owned],
            ret: Box::new(IrType::Unknown),
        };
        let handler = self.lower_expr_with_expected(&args[1], Some(&expected))?;
        let (arity, returns_result) = match &handler.ty {
            IrType::Closure { params, ret, .. } => {
                (params.len(), matches!(ret.as_ref(), IrType::Result(_)))
            }
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
            ty: IrType::Null,
        })
    }

    fn field_type(&self, target: &IrType, field_name: &str) -> IrType {
        let IrType::Named(struct_name) = target else {
            return IrType::Unknown;
        };
        if struct_name == "__ku_error_type" && matches!(field_name, "domain" | "code" | "message") {
            return IrType::Str;
        }
        if struct_name == TIME_TYPE {
            return match field_name {
                "kind" => IrType::Str,
                "millis" => IrType::Int,
                _ => IrType::Unknown,
            };
        }
        if struct_name == HTTP_REQUEST_TYPE && matches!(field_name, "method" | "path" | "body") {
            return IrType::Str;
        }
        // Stage 8b: `req.params` / `req.query` / `req.headers` are dynamic string
        // maps backed by the native `KuObject` ABI. Typing them as `__ku_object`
        // lets `req.query.get_or(...)` dispatch and marks the program as using the
        // object runtime (see `program_uses_object`).
        if struct_name == HTTP_REQUEST_TYPE && matches!(field_name, "params" | "query" | "headers")
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

    /// Reuse storage only in an exact local/cell self-push assignment. The
    /// general array.push operation remains pure. The shared AST predicate
    /// rules out callbacks, fallible calls and references to this receiver, so
    /// lowering the argument first cannot change the receiver snapshot. A cell
    /// payload is cleared by the reuse helper before the following CellStore,
    /// which then stores the returned sole owner into that same cell.
    fn lower_local_array_append(&mut self, name: &str, expr: &Expr) -> KuResult<Option<IrExpr>> {
        let cell = self.assignment_cell(name);
        let ty = match cell.as_ref().map(|cell| &cell.ty) {
            Some(IrType::Cell(inner)) => (**inner).clone(),
            Some(_) => IrType::Unknown,
            None => self
                .locals
                .get(self.local_ir_name(name))
                .cloned()
                .unwrap_or(IrType::Unknown),
        };
        if !matches!(ty, IrType::Array(_)) {
            return Ok(None);
        }
        let ExprKind::Call { callee, args } = &expr.kind else {
            return Ok(None);
        };
        let ExprKind::Field {
            target,
            name: method,
        } = &callee.kind
        else {
            return Ok(None);
        };
        if method != "push"
            || !matches!(&target.kind, ExprKind::Variable(receiver) if receiver == name)
            || args.len() != 1
            || !is_pure_append_argument(&args[0], name)
        {
            return Ok(None);
        }
        let mut value = self.lower_expr(&args[0])?;
        // As with ordinary push, the appended value is cloned, not consumed.
        // Fresh owned rvalues need a cleanup temp, while places stay borrowed.
        if ir_type_is_owned(&value.ty) && !ir_expr_is_place(&value) {
            value = self.emit_temp(value)?;
        }
        let receiver = cell.map_or_else(
            || IrExpr {
                kind: IrExprKind::Local(self.local_ir_name(name).to_string()),
                ty: ty.clone(),
            },
            |cell| IrExpr {
                kind: IrExprKind::CellLoad(Box::new(cell)),
                ty: ty.clone(),
            },
        );
        self.emit_collection_reuse("__ku_array_push_reuse", receiver, ty, value)
            .map(Some)
    }

    fn emit_local_collection_reuse(
        &mut self,
        intrinsic: &str,
        name: &str,
        ty: IrType,
        value: IrExpr,
    ) -> KuResult<IrExpr> {
        self.emit_collection_reuse(
            intrinsic,
            IrExpr {
                kind: IrExprKind::Local(self.local_ir_name(name).to_string()),
                ty: ty.clone(),
            },
            ty,
            value,
        )
    }

    fn emit_collection_reuse(
        &mut self,
        intrinsic: &str,
        receiver: IrExpr,
        ty: IrType,
        value: IrExpr,
    ) -> KuResult<IrExpr> {
        self.emit_temp(IrExpr {
            kind: IrExprKind::Call {
                callee: Box::new(IrExpr {
                    kind: IrExprKind::Local(intrinsic.to_string()),
                    ty: IrType::Function,
                }),
                args: vec![receiver, value],
                kind: IrCallKind::Intrinsic(intrinsic.to_string()),
            },
            ty,
        })
    }

    fn snapshot_receiver_before_effects(
        &mut self,
        receiver: IrExpr,
        has_effects: bool,
    ) -> KuResult<IrExpr> {
        if !has_effects || !ir_type_is_owned(&receiver.ty) || ir_expr_is_borrowed(&receiver) {
            return Ok(receiver);
        }
        // Merely materializing a CellLoad/header would still alias storage that
        // an argument callback can overwrite or free. This temp is a full owner
        // and participates in ordinary cleanup, including a later argument's ?.
        self.emit_temp(IrExpr {
            ty: receiver.ty.clone(),
            kind: IrExprKind::Call {
                callee: Box::new(IrExpr {
                    kind: IrExprKind::Local("__ku_clone".to_string()),
                    ty: IrType::Function,
                }),
                args: vec![receiver],
                kind: IrCallKind::Intrinsic("__ku_clone".to_string()),
            },
        })
    }

    /// Resolve only the projections whose native layouts are known, without
    /// evaluating their owner. This also covers a temporary returned by a call.
    fn borrow_projection_type(&self, expr: &Expr) -> Option<IrType> {
        match &expr.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Variable(name) => match self.static_place_type(callee) {
                    Some(IrType::Closure { ret, .. }) => Some(*ret),
                    Some(_) => None,
                    None => self.signatures.get(name).map(|s| s.returns.clone()),
                },
                _ => None,
            },
            ExprKind::StructLiteral { name, .. } => Some(IrType::Named(name.clone())),
            ExprKind::Field { target, name } => {
                Some(self.field_type(&self.borrow_projection_type(target)?, name))
            }
            ExprKind::Index { target, .. } => match self.borrow_projection_type(target)? {
                IrType::Array(element) => Some(*element),
                _ => None,
            },
            _ => self.static_place_type(expr),
        }
    }

    fn lower_borrow_argument(
        &mut self,
        expr: &Expr,
        expected: Option<&IrType>,
        deferred_safepoint: &mut bool,
    ) -> KuResult<IrExpr> {
        match &expr.kind {
            ExprKind::Field { target, name } if matches!(self.borrow_projection_type(target), Some(IrType::Named(ref n)) if self.layouts.structs.iter().any(|s| &s.name == n)) =>
            {
                let mut target = self.lower_borrow_argument(target, None, deferred_safepoint)?;
                if !ir_expr_is_place(&target) && !ir_expr_is_borrowed(&target) {
                    target = self.emit_temp(target)?;
                }
                let ty = self.field_type(&target.ty, name);
                Ok(IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(target),
                        name: name.clone(),
                    },
                    ty,
                })
            }
            ExprKind::Index { target, index }
                if matches!(self.borrow_projection_type(target), Some(IrType::Array(_)))
                    && is_pure_append_argument(index, "") =>
            {
                let mut target = self.lower_borrow_argument(target, None, deferred_safepoint)?;
                if !ir_expr_is_place(&target) && !ir_expr_is_borrowed(&target) {
                    target = self.emit_temp(target)?;
                }
                let IrType::Array(element) = &target.ty else {
                    unreachable!()
                };
                let ty = *element.clone();
                let index = self.lower_expr(index)?;
                let index = self.emit_temp(index)?;
                Ok(IrExpr {
                    kind: IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    ty,
                })
            }
            _ => self.lower_expr_with_expected_impl(expr, expected, Some(deferred_safepoint)),
        }
    }

    /// Copy arguments and borrowed function bindings must be read before later
    /// callbacks run. Owned arguments retain their existing consuming path;
    /// eagerly moving a function binding would violate its borrowed-call ABI.
    fn lower_call_arguments(
        &mut self,
        args: &[Expr],
        expected: Option<&[IrType]>,
        modes: Option<&[ParamMode]>,
        callee_has_effects: bool,
    ) -> KuResult<Vec<IrExpr>> {
        let first_argument_temp = self.next_temp_id;
        let effects = args
            .iter()
            .map(|arg| !is_pure_append_argument(arg, ""))
            .collect::<Vec<_>>();
        let mut remaining_effects = effects.iter().filter(|effects| **effects).count();
        let mut values = Vec::with_capacity(args.len());
        for (index, (arg, has_effects)) in args.iter().zip(effects).enumerate() {
            remaining_effects -= usize::from(has_effects);
            let view = modes.and_then(|m| m.get(index)) == Some(&ParamMode::View);
            let mut deferred_safepoint = false;
            let mut value = if view {
                self.lower_borrow_argument(
                    arg,
                    expected.and_then(|params| params.get(index)),
                    &mut deferred_safepoint,
                )?
            } else {
                self.lower_expr_with_expected(arg, expected.and_then(|params| params.get(index)))?
            };
            if view {
                if !ir_type_is_owned(&value.ty) && (remaining_effects != 0 || callee_has_effects) {
                    value = self.emit_temp(value)?;
                }
                // Non-place expressions need a real owner until the call returns.
                // Owned temps are cleaned on normal, Result and finally exits.
                if !ir_expr_is_place(&value)
                    && !ir_expr_is_borrowed(&value)
                    && ir_type_is_owned(&value.ty)
                {
                    value = self.emit_temp(value)?;
                }
                if let Some(owner) = borrow_temporary_owner(&value, first_argument_temp) {
                    if !self
                        .pending_borrow_temporaries
                        .iter()
                        .any(|pending| pending.owner.kind == owner.kind)
                    {
                        self.pending_borrow_temporaries
                            .push(PendingBorrowTemporary {
                                owner: owner.clone(),
                                error_block: self
                                    .try_handlers
                                    .last()
                                    .map(|handler| handler.error_block),
                            });
                    }
                }
                if deferred_safepoint {
                    // The root-returning call has completed, but its result was
                    // not registered while that call was being lowered. Poll
                    // only after registration so timeout-finally also drops it.
                    self.emit_safepoint();
                }
                values.push(IrExpr {
                    ty: value.ty.clone(),
                    kind: IrExprKind::Borrow(Box::new(value)),
                });
                continue;
            }
            if remaining_effects != 0 || callee_has_effects {
                if matches!(value.ty, IrType::Closure { .. }) {
                    value = self.snapshot_receiver_before_effects(value, true)?;
                } else if !ir_type_is_owned(&value.ty) {
                    value = self.emit_temp(value)?;
                }
            }
            values.push(value);
        }
        Ok(values)
    }

    fn finish_borrowing_call(
        &mut self,
        call: IrExpr,
        first_argument_temp: usize,
        deferred_safepoint: Option<&mut bool>,
    ) -> KuResult<IrExpr> {
        // The call is fully evaluated now. Nested calls remove only their own
        // newer roots; an outer call's earlier arguments remain pending.
        self.pending_borrow_temporaries.retain(|pending| {
            matches!(pending.owner.kind, IrExprKind::Temp(id) if id.0 < first_argument_temp)
        });
        let mut cleanup = Vec::new();
        if let IrExprKind::Call { args, .. } = &call.kind {
            for arg in args {
                let IrExprKind::Borrow(value) = &arg.kind else {
                    continue;
                };
                if let Some(owner) = borrow_temporary_owner(value, first_argument_temp) {
                    if !cleanup
                        .iter()
                        .any(|existing: &IrExpr| existing.kind == owner.kind)
                    {
                        cleanup.push(owner.clone());
                    }
                }
            }
        }
        if cleanup.is_empty() {
            return self.emit_temp_for_borrow_result(call, deferred_safepoint);
        }
        let needs_safepoint = call.ty == IrType::Void || ir_expr_needs_post_call_safepoint(&call);
        let result = if call.ty == IrType::Void {
            self.current.instructions.push(IrInst::Expr(call));
            IrExpr {
                kind: IrExprKind::Literal("0".into()),
                ty: IrType::Void,
            }
        } else {
            self.emit_temp_with_safepoint(call, false)?
        };
        for owner in cleanup {
            self.emit_borrow_temporary_drop(owner);
        }
        if needs_safepoint {
            if let Some(deferred) = deferred_safepoint {
                *deferred = true;
            } else {
                self.emit_safepoint();
            }
        }
        Ok(result)
    }

    fn emit_borrow_temporary_drop(&mut self, owner: IrExpr) {
        self.current.instructions.push(IrInst::Expr(IrExpr {
            ty: IrType::Void,
            kind: IrExprKind::Call {
                callee: Box::new(IrExpr {
                    kind: IrExprKind::Local("__ku_drop_borrow_temp".into()),
                    ty: IrType::Function,
                }),
                args: vec![owner],
                kind: IrCallKind::Intrinsic("__ku_drop_borrow_temp".into()),
            },
        }));
    }

    /// Recognize receiver-typed stdlib methods without executing a prospective
    /// callee. User functions and module-qualified calls retain their old path.
    fn lower_builtin_method(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        deferred_safepoint: Option<&mut bool>,
    ) -> KuResult<Option<IrExpr>> {
        let ExprKind::Field { target, name } = &callee.kind else {
            return Ok(None);
        };
        let force_kuvalue = matches!(name.as_str(), "as_int" | "as_str");
        let module = if force_kuvalue {
            // The checker guarantees these converters have a KuValue receiver,
            // including a fallible index expression whose static type is absent.
            "kuvalue"
        } else if is_pure_path(target) {
            match self.static_place_type(target) {
                Some(IrType::Array(_)) => "array",
                Some(IrType::Str) => "string",
                Some(IrType::Named(n)) if n == "__ku_object" => "object",
                Some(IrType::Named(n)) if n == "__ku_value" => "kuvalue",
                Some(IrType::Named(n)) if n == metadata::REDIS_CLIENT => "redis",
                Some(IrType::Named(n)) if n == metadata::BYTES => "bytes",
                Some(IrType::Named(n)) if n == metadata::NET_CLIENT => "net",
                Some(IrType::Named(n))
                    if n == "__ku_pg_client" && matches!(name.as_str(), "query" | "close") =>
                {
                    "pg_client"
                }
                Some(IrType::Named(n))
                    if n == "__ku_pg_result"
                        && matches!(name.as_str(), "rows" | "cols" | "value" | "is_null") =>
                {
                    "pg_result"
                }
                Some(IrType::Named(n))
                    if (n == metadata::MYSQL_CLIENT
                        && matches!(name.as_str(), "query" | "execute" | "close"))
                        || (n == metadata::MYSQL_RESULT
                            && matches!(name.as_str(), "rows" | "cols" | "value" | "is_null")) =>
                {
                    "mysql"
                }
                _ => return Ok(None),
            }
        } else if self.static_receiver_type(target, 0) == Some(IrType::Str) {
            "string"
        } else {
            return Ok(None);
        };
        let signature = if module == "redis" {
            metadata::redis_client_method_signature(name)
        } else if module == "bytes" {
            metadata::bytes_method_signature(name)
        } else if module == "net" {
            metadata::net_client_method_signature(name)
        } else if module == "mysql" {
            let Some(IrType::Named(native)) = self.static_place_type(target) else {
                return Ok(None);
            };
            metadata::mysql_method_signature(&native, name)
        } else {
            metadata::dotted_signature(module, name)
        };
        let Some(signature) = signature else {
            return Ok(None);
        };
        let receiver = self.lower_field_target(target)?;
        let argument_effects = args
            .iter()
            .map(|arg| !is_pure_append_argument(arg, ""))
            .collect::<Vec<_>>();
        let mut remaining_effects = argument_effects.iter().filter(|effects| **effects).count();
        // Database receiver methods borrow move-only native handles. Cloning a
        // client/result merely to freeze its pointer before an effectful argument
        // would duplicate ownership and reaches the backend's forbidden-clone
        // trap. The checker requires these receivers to be bound places.
        let receiver = if matches!(
            module,
            "redis" | "net" | "mysql" | "pg_client" | "pg_result"
        ) {
            receiver
        } else {
            self.snapshot_receiver_before_effects(receiver, remaining_effects != 0)?
        };
        let mut lowered_args = Vec::with_capacity(args.len());
        for (index, (arg, has_effects)) in args.iter().zip(argument_effects).enumerate() {
            remaining_effects -= usize::from(has_effects);
            let expected = match signature.args.get(index + 1) {
                Some(ArgRule::Is(pattern)) => pattern_to_ir_type(pattern, &[]),
                _ => None,
            };
            let mut value = self.lower_expr_with_expected(arg, expected.as_ref())?;
            if remaining_effects != 0 {
                // Later callbacks may also replace an earlier argument's
                // binding. Freeze each evaluated value left-to-right, not just
                // the receiver. Pure argument lists need no extra copies.
                value = if ir_type_is_owned(&value.ty) {
                    self.snapshot_receiver_before_effects(value, true)?
                } else {
                    self.emit_temp(value)?
                };
            }
            lowered_args.push(value);
        }
        // push clones its argument, so a fresh owned value needs a cleanup temp
        // while a borrowed variable/field must remain live in its own binding.
        if module == "array" && name == "push" {
            if let Some(value) = lowered_args.first() {
                if ir_type_is_owned(&value.ty) && !ir_expr_is_place(value) {
                    lowered_args[0] = self.emit_temp(value.clone())?;
                }
            }
        }
        let mut all_args = Vec::with_capacity(lowered_args.len() + 1);
        all_args.push(receiver);
        all_args.extend(lowered_args);
        let ty = if module == "object" {
            IrType::Named("__ku_value".to_string())
        } else {
            signature_return_type(&signature, &all_args)
        };
        self.emit_temp_for_borrow_result(
            IrExpr {
                kind: IrExprKind::Call {
                    callee: Box::new(IrExpr {
                        kind: IrExprKind::Local(format!("{module}.{name}")),
                        ty: IrType::Function,
                    }),
                    args: all_args,
                    kind: IrCallKind::Intrinsic(format!("{module}.{name}")),
                },
                ty,
            },
            deferred_safepoint,
        )
        .map(Some)
    }

    fn lower_expr(&mut self, expr: &Expr) -> KuResult<IrExpr> {
        self.lower_expr_impl(expr, None)
    }

    fn lower_expr_impl(
        &mut self,
        expr: &Expr,
        mut deferred_safepoint: Option<&mut bool>,
    ) -> KuResult<IrExpr> {
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
                let name = self.local_ir_name(name);
                if self.borrowed_params.contains(name) {
                    return Ok(IrExpr {
                        kind: IrExprKind::BorrowedParam(name.to_string()),
                        ty: self.locals.get(name).cloned().unwrap_or(IrType::Unknown),
                    });
                }
                // Stage 6b: a boxed local read in the scope that owns the cell.
                if let Some(IrType::Cell(inner)) = self.locals.get(name) {
                    let inner = (**inner).clone();
                    return Ok(self.cell_load(IrExprKind::Local(name.to_string()), inner));
                }
                // A local declaration shadows an identically spelled capture.
                if !self.locals.contains_key(name) {
                    if let Some(IrType::Cell(inner)) = self.captures.get(name) {
                        let inner = (**inner).clone();
                        return Ok(
                            self.cell_load(IrExprKind::CapturedCell(name.to_string()), inner)
                        );
                    }
                }
                // A top-level function name used as a value (not as a direct call
                // callee — those are intercepted in Call lowering) lowers to a
                // closure over that function via its `__thunk` (Stage 6a).
                if !self.locals.contains_key(name) {
                    if let Some(signature) = self.signatures.get(name) {
                        let params = signature.params.clone();
                        let param_modes = signature.param_modes.clone();
                        let ret = Box::new(signature.returns.clone());
                        return Ok(IrExpr {
                            kind: IrExprKind::MakeClosure {
                                function_id: signature.id,
                                captures: Vec::new(),
                            },
                            ty: IrType::Closure {
                                params,
                                param_modes,
                                ret,
                            },
                        });
                    }
                }
                Ok(IrExpr {
                    kind: IrExprKind::Local(name.to_string()),
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
            ExprKind::Binary { left, op, right } if matches!(op, BinaryOp::And | BinaryOp::Or) => {
                self.lower_logical_expr(left, *op, right)
            }
            ExprKind::Binary { left, op, right } => {
                let mut left = self.lower_expr(left)?;
                if !is_pure_append_argument(right, "") {
                    if left.ty == IrType::Str {
                        left = self.snapshot_receiver_before_effects(left, true)?;
                    } else if !ir_type_is_owned(&left.ty) {
                        left = self.emit_temp(left)?;
                    }
                }
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
                        let first_argument_temp = self.next_temp_id;
                        let mut lowered_args = Vec::with_capacity(args.len() + 1);
                        lowered_args.push(IrExpr {
                            kind: IrExprKind::Local("__env".to_string()),
                            ty: IrType::Unknown,
                        });
                        let modes = self.self_param_modes.clone();
                        lowered_args.extend(self.lower_call_arguments(
                            args,
                            None,
                            Some(&modes),
                            false,
                        )?);
                        return self.finish_borrowing_call(
                            IrExpr {
                                kind: IrExprKind::Call {
                                    callee: Box::new(IrExpr {
                                        kind: IrExprKind::Local(format!(
                                            "__ku_closure_{}",
                                            self_id.0
                                        )),
                                        ty: IrType::Function,
                                    }),
                                    args: lowered_args,
                                    kind: IrCallKind::Direct(self_id),
                                },
                                ty: ret_ty,
                            },
                            first_argument_temp,
                            deferred_safepoint,
                        );
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
                        // A mapper (or its factory) can replace a captured source
                        // binding. Keep an owned snapshot until iteration and its
                        // callbacks finish instead of borrowing a freed buffer.
                        let receiver = self.snapshot_receiver_before_effects(receiver, true)?;
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
                            param_modes: vec![ParamMode::Owned],
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
                        "get" | "post" | "put" | "del" | "listen" | "bind"
                    ) && is_pure_path(target)
                    {
                        let receiver = self.lower_expr(target)?;
                        if matches!(&receiver.ty, IrType::Named(n) if n == HTTP_SERVER_TYPE) {
                            return self.lower_http_server_method(receiver, name, args, expr.span);
                        }
                    }
                }
                if let Some(value) =
                    self.lower_builtin_method(callee, args, deferred_safepoint.as_deref_mut())?
                {
                    return Ok(value);
                }
                // A lexical function binding takes precedence over a builtin
                // or top-level function with the same name. Its parameters also
                // provide context for closure/Result constructor arguments.
                let bound_callee = match &callee.kind {
                    ExprKind::Variable(_) => self.static_place_type(callee),
                    ExprKind::Field { .. } | ExprKind::Index { .. } => self
                        .static_place_type(callee)
                        .filter(|ty| matches!(ty, IrType::Closure { .. })),
                    _ => None,
                };
                let expected_param_types = match &bound_callee {
                    Some(IrType::Closure { params, .. }) => Some(params.clone()),
                    Some(_) => None,
                    None => match &callee.kind {
                        ExprKind::Variable(name) => {
                            self.signatures.get(name).map(|sig| sig.params.clone())
                        }
                        _ => None,
                    },
                };
                let first_argument_temp = self.next_temp_id;
                let lowered_args = self.lower_call_arguments(
                    args,
                    expected_param_types.as_deref(),
                    match &bound_callee {
                        Some(IrType::Closure { param_modes, .. }) => Some(param_modes.clone()),
                        Some(_) => None,
                        None => match &callee.kind {
                            ExprKind::Variable(name) => self
                                .signatures
                                .get(name)
                                .map(|s| s.param_modes.clone())
                                .or_else(|| metadata::builtin_signature(name).map(|s| s.arg_modes)),
                            ExprKind::Field { .. } => dotted_name(callee).and_then(|n| {
                                n.split_once('.')
                                    .and_then(|(m, f)| metadata::dotted_signature(m, f))
                                    .map(|s| s.arg_modes)
                            }),
                            _ => None,
                        },
                    }
                    .as_deref(),
                    !is_pure_append_argument(callee, ""),
                )?;
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
                let (kind, mut ty) = if bound_callee.is_some() {
                    (IrCallKind::Indirect, IrType::Unknown)
                } else {
                    call_kind_and_type(callee, &lowered_args, self.signatures)
                };
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
                self.finish_borrowing_call(
                    IrExpr {
                        kind: IrExprKind::Call {
                            callee: Box::new(callee),
                            args: lowered_args,
                            kind,
                        },
                        ty,
                    },
                    first_argument_temp,
                    deferred_safepoint,
                )
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
                let target = self.snapshot_receiver_before_effects(
                    target,
                    !is_pure_append_argument(index, ""),
                )?;
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
                if ir_expr_is_borrowed(&field) {
                    Ok(field)
                } else if ir_type_is_owned(&ty) {
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
                        Ok((
                            field.clone(),
                            self.lower_expr_with_expected(value, expected)?,
                        ))
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

    /// Lower source `&&` / `||` into control flow instead of first materializing
    /// both operands as ordinary expression temporaries. Calls, indexes and `?`
    /// on the right can emit instructions (and even their own blocks), so leaving
    /// the logical operator as a final C expression would evaluate those effects
    /// before C ever reached its built-in short-circuit operator.
    fn lower_logical_expr(&mut self, left: &Expr, op: BinaryOp, right: &Expr) -> KuResult<IrExpr> {
        debug_assert!(matches!(op, BinaryOp::And | BinaryOp::Or));

        let left = self.lower_expr(left)?;
        let result_name = format!("__ku_logical_{}", self.next_temp_id);
        self.next_temp_id += 1;
        let right_id = self.next_block("logical_right");
        let after_id = self.next_block("logical_after");
        let short_value = op == BinaryOp::Or;

        // This declaration dominates both successors. The skipped edge keeps the
        // operator's decisive value (`false` for &&, `true` for ||); only the RHS
        // edge overwrites it with the value it actually evaluates.
        self.current.instructions.push(IrInst::Let {
            name: result_name.clone(),
            ty: IrType::Bool,
            value: bool_literal(short_value),
        });
        let (then_block, else_block) = if op == BinaryOp::And {
            (right_id, after_id)
        } else {
            (after_id, right_id)
        };
        self.current.terminator = IrTerminator::Branch {
            condition: left,
            then_block,
            else_block,
        };
        self.finish_current();

        self.start_block(right_id, "logical_right");
        let right = self.lower_expr(right)?;
        self.current.instructions.push(IrInst::Store {
            target: IrLValue::Local(result_name.clone()),
            value: right,
        });
        if self.current.terminator == IrTerminator::Next {
            self.current.terminator = IrTerminator::Jump(after_id);
        }
        self.finish_current();

        self.start_block(after_id, "logical_after");
        Ok(IrExpr {
            kind: IrExprKind::Local(result_name),
            ty: IrType::Bool,
        })
    }

    /// Like [`lower_expr`], but threads an expected type from context into
    /// aggregate and closure literals, including the payload of Result
    /// constructors. This preserves the element type of an empty array, the
    /// success type of `err(message)`, and unannotated closure parameter types.
    /// Mirrors the checker's `check_expr_expecting` so values the checker
    /// accepts keep the same concrete type in native IR (rule 8).
    fn lower_expr_with_expected(
        &mut self,
        expr: &Expr,
        expected: Option<&IrType>,
    ) -> KuResult<IrExpr> {
        self.lower_expr_with_expected_impl(expr, expected, None)
    }

    fn lower_expr_with_expected_impl(
        &mut self,
        expr: &Expr,
        expected: Option<&IrType>,
        deferred_safepoint: Option<&mut bool>,
    ) -> KuResult<IrExpr> {
        if let (ExprKind::Call { callee, args }, Some(IrType::Result(expected_inner))) =
            (&expr.kind, expected)
        {
            if let ExprKind::Variable(name) = &callee.kind {
                if matches!(name.as_str(), "ok" | "err")
                    && args.len() == 1
                    && **expected_inner != IrType::Unknown
                    && !self.signatures.contains_key(name)
                    && self.static_place_type(callee).is_none()
                {
                    let value = if name == "ok" {
                        self.lower_expr_with_expected(&args[0], Some(expected_inner))?
                    } else {
                        self.lower_expr(&args[0])?
                    };
                    return self.emit_temp_for_borrow_result(
                        IrExpr {
                            kind: IrExprKind::Call {
                                callee: Box::new(IrExpr {
                                    kind: IrExprKind::Local(name.clone()),
                                    ty: IrType::Function,
                                }),
                                args: vec![value],
                                kind: IrCallKind::Intrinsic(name.clone()),
                            },
                            ty: IrType::Result(expected_inner.clone()),
                        },
                        deferred_safepoint,
                    );
                }
            }
        }
        if let (ExprKind::Array(values), Some(IrType::Array(expected_element))) =
            (&expr.kind, expected)
        {
            let values = values
                .iter()
                .map(|value| self.lower_expr_with_expected(value, Some(expected_element)))
                .collect::<KuResult<Vec<_>>>()?;
            return self.emit_temp(IrExpr {
                kind: IrExprKind::Array(values),
                ty: IrType::Array(expected_element.clone()),
            });
        }
        if let ExprKind::Function { params, body, .. } = &expr.kind {
            let expected_params = match expected {
                Some(IrType::Closure { params, .. }) => Some(params.as_slice()),
                _ => None,
            };
            return self.lower_closure_literal(params, body, expr.span, expected_params);
        }
        self.lower_expr_impl(expr, deferred_safepoint)
    }

    /// Resolve the cell pointer a newly-created nested closure must retain.
    /// Locals take lexical precedence over an identically named outer capture;
    /// otherwise a closure body forwards the cell already stored in its `__env`.
    fn capture_binding(
        &self,
        name: &str,
        span: Span,
    ) -> KuResult<Option<(IrType, IrCaptureSource)>> {
        let source_name = name;
        // Match bindings live in their own expression overlay rather than the
        // ordinary local/type table, and they lexically shadow every homonym.
        // Check that overlay first: otherwise an outer boxed local with the same
        // spelling would be captured and the native program would return the
        // wrong value instead of rejecting this not-yet-supported capture form.
        if self.pattern_bindings.contains_key(source_name) {
            return Err(KuError::runtime(
                format!(
                    "IR/native lowering does not support closure capture of match binding '{source_name}' yet"
                ),
                span,
            ));
        }
        let name = self.local_ir_name(source_name);
        if let Some(local) = self.locals.get(name) {
            return match local {
                IrType::Cell(_) => Ok(Some((local.clone(), IrCaptureSource::Local))),
                _ => Err(KuError::runtime(
                    format!(
                        "IR/native lowering cannot capture lexical local '{source_name}' without a shared cell"
                    ),
                    span,
                )),
            };
        }
        if let Some(capture) = self.captures.get(name) {
            return match capture {
                IrType::Cell(_) => Ok(Some((
                    capture.clone(),
                    IrCaptureSource::EnclosingEnvironment,
                ))),
                _ => Err(KuError::runtime(
                    format!(
                        "IR/native lowering cannot forward lexical capture '{source_name}' without a shared cell"
                    ),
                    span,
                )),
            };
        }
        if self
            .self_recurse
            .as_ref()
            .is_some_and(|(self_name, _, _)| self_name == source_name)
        {
            return Err(KuError::runtime(
                format!(
                    "IR/native lowering does not support nested closure capture of local function self '{source_name}' yet"
                ),
                span,
            ));
        }
        Ok(None)
    }

    fn lower_capture_bindings(
        &self,
        names: HashSet<String>,
        span: Span,
    ) -> KuResult<LoweredCaptureBindings> {
        let mut values = Vec::with_capacity(names.len());
        let mut aliases = HashMap::with_capacity(names.len());
        // Free-name discovery returns a HashSet. Resolve in sorted order too,
        // not only emit in sorted order, so the first fail-closed diagnostic is
        // stable across processes and platforms when several names are invalid.
        let mut names = names.into_iter().collect::<Vec<_>>();
        names.sort();
        for name in names {
            if let Some((ty, source)) = self.capture_binding(&name, span)? {
                let ir_name = self.local_ir_name(&name).to_string();
                if ir_name != name {
                    aliases.insert(name, ir_name.clone());
                }
                values.push((ir_name, ty, source));
            }
        }
        // Aliases can change the IR spelling, so retain the existing ABI order
        // sort after resolution as well.
        values.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(LoweredCaptureBindings { values, aliases })
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
                mode: param.mode,
            });
        }

        // Stage 6b: the cells this closure captures = its free variables that
        // are boxed cells in the enclosing scope (a boxed local, or — for nested
        // closures — a cell already captured here). Sorted for a stable env-field
        // and argument order shared by the body and every `MakeClosure`.
        let LoweredCaptureBindings {
            values: captures,
            aliases,
        } = self.lower_capture_bindings(
            crate::runtime::interpreter::closure_capture_names(params, body),
            span,
        )?;
        let function_captures = captures
            .iter()
            .map(|(name, ty, _)| (name.clone(), ty.clone()))
            .collect::<Vec<_>>();

        let mut child = FunctionLowerer::new(
            self.signatures,
            self.layouts,
            IrType::Unknown,
            self.next_function_id.clone(),
            self.lifted_functions.clone(),
        );
        child.captures = function_captures.iter().cloned().collect();
        child.local_names = aliases;
        for param in &ir_params {
            child.locals.insert(param.name.clone(), param.ty.clone());
        }
        child.lower_block_body("entry", body, span, params)?;

        // Recover the real return type from a source return. Cooperative timeout
        // blocks are lowered while this child still has an Unknown seed, so their
        // synthetic zero returns must not win inference over the closure body's
        // concrete return.
        let return_type = child
            .blocks
            .iter()
            .find_map(|block| match &block.terminator {
                IrTerminator::Return(Some(value)) if value.ty != IrType::Unknown => {
                    Some(value.ty.clone())
                }
                _ => None,
            })
            .unwrap_or(IrType::Null);
        resolve_closure_safepoint_return_type(&mut child.blocks, &return_type);

        let cid = FunctionId(self.next_function_id.get());
        self.next_function_id.set(cid.0 + 1);
        let param_types = ir_params.iter().map(|param| param.ty.clone()).collect();
        let param_modes = ir_params.iter().map(|param| param.mode).collect();
        self.lifted_functions.borrow_mut().push(IrFunction {
            id: cid,
            name: format!("__ku_closure_{}", cid.0),
            params: ir_params,
            return_type: return_type.clone(),
            blocks: child.blocks,
            is_closure_body: true,
            captures: function_captures,
        });

        self.emit_temp(IrExpr {
            kind: IrExprKind::MakeClosure {
                function_id: cid,
                captures,
            },
            ty: IrType::Closure {
                params: param_types,
                param_modes,
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
                mode: param.mode,
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
        let LoweredCaptureBindings {
            values: captures,
            aliases,
        } = self.lower_capture_bindings(
            crate::runtime::interpreter::function_capture_names(function),
            function.span,
        )?;
        let function_captures = captures
            .iter()
            .map(|(name, ty, _)| (name.clone(), ty.clone()))
            .collect::<Vec<_>>();

        let mut child = FunctionLowerer::new(
            self.signatures,
            self.layouts,
            return_type.clone(),
            self.next_function_id.clone(),
            self.lifted_functions.clone(),
        );
        child.captures = function_captures.iter().cloned().collect();
        child.local_names = aliases;
        for param in &ir_params {
            child.locals.insert(param.name.clone(), param.ty.clone());
        }
        // Wire self-recursion: a call to `name` in the body reuses the running env.
        child.self_recurse = Some((name.clone(), cid, return_type.clone()));
        child.self_param_modes = ir_params.iter().map(|p| p.mode).collect();
        child.lower_block_body("entry", &function.body, function.span, &function.params)?;

        let param_modes = ir_params.iter().map(|p| p.mode).collect();
        self.lifted_functions.borrow_mut().push(IrFunction {
            id: cid,
            name: format!("__ku_closure_{}", cid.0),
            params: ir_params,
            return_type: return_type.clone(),
            blocks: child.blocks,
            is_closure_body: true,
            captures: function_captures,
        });

        // Bind the name in the enclosing scope as a first-class closure value.
        let closure_ty = IrType::Closure {
            params: param_types,
            param_modes,
            ret: Box::new(return_type),
        };
        let value = self.emit_temp(IrExpr {
            kind: IrExprKind::MakeClosure {
                function_id: cid,
                captures,
            },
            ty: closure_ty.clone(),
        })?;
        let name = self.define_local(&name, closure_ty.clone());
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
        self.emit_temp_with_safepoint(value, true)
    }

    fn emit_temp_for_borrow_result(
        &mut self,
        value: IrExpr,
        deferred_safepoint: Option<&mut bool>,
    ) -> KuResult<IrExpr> {
        if let Some(deferred) = deferred_safepoint {
            *deferred |= ir_expr_needs_post_call_safepoint(&value);
            self.emit_temp_with_safepoint(value, false)
        } else {
            self.emit_temp(value)
        }
    }

    fn emit_temp_with_safepoint(
        &mut self,
        value: IrExpr,
        post_call_safepoint: bool,
    ) -> KuResult<IrExpr> {
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
        let borrowed = ir_expr_is_borrowed(&value) && ir_type_is_owned(&ty);
        let needs_safepoint = post_call_safepoint && ir_expr_needs_post_call_safepoint(&value);
        self.current.instructions.push(IrInst::Temp {
            id,
            ty: ty.clone(),
            value,
        });
        if needs_safepoint {
            self.emit_safepoint();
        }
        Ok(IrExpr {
            kind: if borrowed {
                IrExprKind::BorrowedTemp(id)
            } else {
                IrExprKind::Temp(id)
            },
            ty,
        })
    }

    /// Split the current block into a deadline branch, an internal timeout-return
    /// block, and a continuation block. Crucially, the timeout edge is produced by
    /// `return_terminator`: if lowering is currently inside a try with finally,
    /// the zero return payload is stored in the existing return slot and control
    /// visits `finally_return` (and then any enclosing finally) before leaving the
    /// frame. The TLS timeout flag remains set so callers repeat this structured
    /// unwind and the HTTP worker alone emits 504.
    fn emit_safepoint(&mut self) {
        let continue_block = self.next_block("safepoint_continue");
        let timeout_block = self.next_block("safepoint_timeout");
        self.current.terminator = IrTerminator::Safepoint {
            continue_block,
            timeout_block,
        };
        self.finish_current();

        self.start_block(timeout_block, "safepoint_timeout");
        // A timeout abandons every active argument evaluation in this frame.
        // Release only its fresh borrowed roots before entering user finally;
        // the ordinary success edge still keeps those roots until its call.
        let abandoned = self
            .pending_borrow_temporaries
            .iter()
            .rev()
            .map(|pending| pending.owner.clone())
            .collect::<Vec<_>>();
        for owner in abandoned {
            self.emit_borrow_temporary_drop(owner);
        }
        let timeout_value =
            (self.return_type != IrType::Void).then(|| zero_expr(self.return_type.clone()));
        self.current.terminator = self.return_terminator(timeout_value);
        self.finish_current();

        self.start_block(continue_block, "safepoint_continue");
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
        let name = self.local_ir_name(name);
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
        match self.locals.get(self.local_ir_name(name)) {
            Some(IrType::Cell(inner)) => Some((**inner).clone()),
            _ => None,
        }
    }

    fn assignment_cell(&self, name: &str) -> Option<IrExpr> {
        let name = self.local_ir_name(name);
        if self.locals.contains_key(name) {
            self.boxed_local_inner(name).map(|inner| IrExpr {
                kind: IrExprKind::Local(name.to_string()),
                ty: IrType::Cell(Box::new(inner)),
            })
        } else if self.captures.contains_key(name) {
            Some(self.captured_cell_expr(name))
        } else {
            None
        }
    }

    fn binding_is_boxed(&self, name: &str, span: Span) -> bool {
        self.boxed.contains(&BoxedBindingSite::new(name, span))
    }

    /// Store an assignment result using the same binding/cell rules for plain
    /// and parallel destructuring assignment. Callers decide when to materialize
    /// the RHS; destructuring evaluates every RHS temp before invoking this
    /// helper, preserving its parallel-assignment semantics.
    fn store_or_define_name(
        &mut self,
        name: &str,
        value: IrExpr,
        span: Span,
        define_boxed: bool,
    ) -> KuResult<()> {
        if let Some(cell) = self.assignment_cell(name) {
            self.current
                .instructions
                .push(IrInst::CellStore { cell, value });
        } else if define_boxed && !self.locals.contains_key(self.local_ir_name(name)) {
            // First assignment to a to-be-boxed local: allocate its cell.
            let inner = value.ty.clone();
            self.push_cell_new(name.to_string(), inner, value, span)?;
        } else if self.locals.contains_key(self.local_ir_name(name)) {
            self.current.instructions.push(IrInst::Store {
                target: IrLValue::Local(self.local_ir_name(name).to_string()),
                value,
            });
        } else {
            let ty = value.ty.clone();
            let name = self.define_local(name, ty.clone());
            self.current
                .instructions
                .push(IrInst::Let { name, ty, value });
        }
        Ok(())
    }

    /// Box a captured local into a fresh cell (rc=1), recording its
    /// `Cell(inner)` type. The native cell owns every payload whose ABI already
    /// has move/drop support, including aggregate/Result/function/KuValue values.
    fn push_cell_new(
        &mut self,
        name: String,
        inner: IrType,
        init: IrExpr,
        span: Span,
    ) -> KuResult<()> {
        let supported = is_copy_ir_type(&inner) || ir_type_is_owned(&inner);
        if !supported {
            return Err(KuError::runtime(
                format!("native closure capture of {inner} is not supported"),
                span,
            ));
        }
        let name = self.define_local(&name, IrType::Cell(Box::new(inner.clone())));
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
            let mut binding_names = bindings.keys().cloned().collect::<Vec<_>>();
            binding_names.sort();
            let pattern_undo = self.push_pattern_bindings(bindings);
            // Materialize each binding that is a computed projection (an enum
            // payload access, which moves-and-clears the slot when its type is
            // owned) into a single temp, so using the binding more than once reads
            // that temp instead of re-moving the value out of the enum each time.
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
                    self.pop_pattern_bindings(pattern_undo);
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
            self.pop_pattern_bindings(pattern_undo);
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
        let error_block = self.try_handlers.last().map(|handler| handler.error_block);
        let aborted = self
            .pending_borrow_temporaries
            .iter()
            .rev()
            .filter(|pending| pending.error_block == error_block)
            .map(|pending| pending.owner.clone())
            .collect::<Vec<_>>();
        for owner in aborted {
            self.emit_borrow_temporary_drop(owner);
        }
        // Do not remove compile-time records here: the sibling success edge
        // still owns them and emits its normal post-call cleanup.
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

/// Calls into Ku code can discover a cooperative handler timeout in a deeper
/// frame, so their caller must poll immediately afterward and continue the
/// structured unwind. `array.map` is an intrinsic, but invokes a Ku closure from
/// its generated loop and participates in the same protocol.
fn ir_expr_needs_post_call_safepoint(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::Call {
            kind: IrCallKind::Direct(_) | IrCallKind::Indirect,
            ..
        } => true,
        IrExprKind::Call {
            kind: IrCallKind::Intrinsic(name),
            ..
        } => name == "array.map",
        _ => false,
    }
}

/// Closure bodies are initially lowered with an Unknown return seed so their
/// source returns can drive inference. Safepoint timeout paths created during
/// that pass therefore contain Unknown-typed zero payloads (and a try/finally
/// return slot may be Unknown too). Once inference succeeds, make only those
/// synthetic return artifacts concrete; ordinary Unknown expressions retain
/// their diagnostic meaning.
fn resolve_closure_safepoint_return_type(blocks: &mut [IrBlock], return_type: &IrType) {
    const RETURN_SLOT_PREFIX: &str = "__ku_return_";

    for block in blocks {
        for instruction in &mut block.instructions {
            match instruction {
                IrInst::Let { name, ty, value }
                    if name.starts_with(RETURN_SLOT_PREFIX) && *ty == IrType::Unknown =>
                {
                    *ty = return_type.clone();
                    if is_unknown_native_zero(value) {
                        *value = zero_expr(return_type.clone());
                    }
                }
                IrInst::Store {
                    target: IrLValue::Local(name),
                    value,
                } if name.starts_with(RETURN_SLOT_PREFIX) && is_unknown_native_zero(value) => {
                    *value = zero_expr(return_type.clone());
                }
                _ => {}
            }
        }

        if let IrTerminator::Return(Some(value)) = &mut block.terminator {
            let timeout_zero =
                block.name.starts_with("safepoint_timeout") && is_unknown_native_zero(value);
            let return_slot = matches!(
                &value.kind,
                IrExprKind::Local(name) if name.starts_with(RETURN_SLOT_PREFIX)
            ) && value.ty == IrType::Unknown;
            if timeout_zero {
                *value = zero_expr(return_type.clone());
            } else if return_slot {
                value.ty = return_type.clone();
            }
        }
    }
}

fn is_unknown_native_zero(expr: &IrExpr) -> bool {
    expr.ty == IrType::Unknown
        && matches!(&expr.kind, IrExprKind::Literal(value) if value == "<native-zero>")
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
            param_modes,
            return_type,
            ..
        } => IrType::Closure {
            params: params.iter().map(|p| lower_type(p, layouts)).collect(),
            param_modes: param_modes.clone(),
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
                param_modes,
                return_type,
                ..
            } => IrType::Closure {
                params: params.iter().map(|p| lower(p, enum_names)).collect(),
                param_modes: param_modes.clone(),
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
                // The checker exposes Time as a dynamic object, but native needs a
                // dedicated type so an unrelated `{ kind, millis }` object cannot
                // accidentally enter the Time ABI. Stage 7 reserves now() for an
                // epoch-millisecond int; instant() constructs the native Time.
                if let Some(ty) = time_builtin_ir_type(module, function, args) {
                    return (IrCallKind::Intrinsic(name), ty);
                }
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
/// Dedicated by-value native Time ABI. Kept separate from `__ku_object` so
/// ordinary dynamic objects with fields named `kind`/`millis` are never
/// misclassified as values returned by `time.instant()`.
const TIME_TYPE: &str = "__ku_time";

fn time_builtin_ir_type(module: &str, function: &str, args: &[IrExpr]) -> Option<IrType> {
    if module != "time" {
        return None;
    }
    match function {
        "instant" if args.is_empty() => Some(IrType::Named(TIME_TYPE.to_string())),
        "now" | "elapsed" | "millis" | "steady_millis" => Some(IrType::Int),
        _ => None,
    }
}

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

/// Copy scalar payloads need no ownership helper when moved into a shared cell.
/// `push_cell_new` separately admits owned ABI types, whose generated cell
/// helpers move/drop the payload.
fn is_copy_ir_type(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null
    ) || matches!(ty, IrType::Named(name) if name == TIME_TYPE)
}

/// A visible name either resolves to a binding site owned and boxable by this
/// function body (`Some`) or to a lexical binding this scan cannot box (`None`).
/// Body declarations and this function's parameters are `Some`; enclosing
/// captures and catch/match bindings are `None`. A loop iterator uses a stable
/// `Some` sentinel only so `lower_for` can reject its unsupported per-iteration
/// capture explicitly. Non-boxable bindings still matter because they shadow an
/// outer homonym.
type VisibleBoxBindings = HashMap<String, Option<BoxedBindingSite>>;

/// Stage 6b: find the exact body-owned bindings captured by closures at their
/// creation points. The scan follows statement order and block scope. It must
/// not recursively merge a closure body's own boxed locals into its parent: the
/// child FunctionLowerer scans that body separately, while the interpreter's
/// free-variable analysis already propagates any genuine transitive capture.
fn collect_boxed_candidates<P: BodyParameter>(
    body: &[Stmt],
    lexical_bindings: &HashSet<String>,
    parameters: &[P],
) -> HashSet<BoxedBindingSite> {
    let mut visible = lexical_bindings
        .iter()
        .cloned()
        .map(|name| (name, None))
        .collect::<VisibleBoxBindings>();
    // Parameters shadow every enclosing homonym, but unlike an enclosing
    // capture they belong to this function and have a stable binding site. A
    // deeper closure that names one must therefore cause an entry CellNew.
    for parameter in parameters {
        let name = parameter.binding_name();
        visible.insert(
            name.to_string(),
            Some(BoxedBindingSite::new(name, parameter.binding_span())),
        );
    }
    let mut out = HashSet::new();
    collect_boxed_candidates_block(body, &mut visible, &mut out);
    out
}

fn collect_boxed_candidates_block(
    body: &[Stmt],
    visible: &mut VisibleBoxBindings,
    out: &mut HashSet<BoxedBindingSite>,
) {
    for stmt in body {
        collect_boxed_candidates_stmt(stmt, visible, out);
        // Follow source-level static fallthrough: these statements terminate
        // the current block, so later declarations/closures are unreachable
        // and must not add entry-time cell allocations to a hot function.
        // Break/continue are still rejected by native lowering today, but
        // treating them as terminators here keeps this pre-scan correct when
        // that lowering is added.
        if matches!(
            stmt,
            Stmt::Return { .. }
                | Stmt::Fail { .. }
                | Stmt::Panic { .. }
                | Stmt::Break { .. }
                | Stmt::Continue { .. }
        ) {
            break;
        }
    }
}

fn record_visible_captures(
    captures: HashSet<String>,
    visible: &VisibleBoxBindings,
    out: &mut HashSet<BoxedBindingSite>,
) {
    for name in captures {
        if let Some(Some(binding)) = visible.get(&name) {
            out.insert(binding.clone());
        }
    }
}

fn define_assignment_binding(name: &str, span: Span, visible: &mut VisibleBoxBindings) {
    visible
        .entry(name.to_string())
        .or_insert_with(|| Some(BoxedBindingSite::new(name, span)));
}

fn collect_boxed_candidates_stmt(
    stmt: &Stmt,
    visible: &mut VisibleBoxBindings,
    out: &mut HashSet<BoxedBindingSite>,
) {
    match stmt {
        Stmt::VarDecl {
            name, value, span, ..
        } => {
            collect_boxed_candidates_expr(value, visible, out);
            // A declaration always creates a new binding in the current block,
            // shadowing any parameter/capture/outer-block homonym.
            visible.insert(name.clone(), Some(BoxedBindingSite::new(name, *span)));
        }
        Stmt::Assign { name, value, span } => {
            collect_boxed_candidates_expr(value, visible, out);
            // Plain assignment defines a local only when no lexical binding is
            // visible. This mirrors Env::contains/assign-or-define.
            define_assignment_binding(name, *span, visible);
        }
        Stmt::AssignTarget { target, value, .. } | Stmt::CompoundAssign { target, value, .. } => {
            // Assignment evaluates its RHS before resolving the destination.
            collect_boxed_candidates_expr(value, visible, out);
            collect_boxed_candidates_assign_target(target, visible, out);
        }
        Stmt::DestructureAssign {
            names,
            values,
            span,
        } => {
            for value in values {
                collect_boxed_candidates_expr(value, visible, out);
            }
            for name in names.iter().flatten() {
                define_assignment_binding(name, *span, visible);
            }
        }
        Stmt::ObjectDestructureAssign {
            bindings,
            rest,
            value,
            span,
        } => {
            collect_boxed_candidates_expr(value, visible, out);
            for binding in bindings {
                if let Some(default) = &binding.default {
                    collect_boxed_candidates_expr(default, visible, out);
                }
                if let Some(local) = &binding.local {
                    define_assignment_binding(local, *span, visible);
                }
            }
            if let Some(local) = rest.as_ref().and_then(|rest| rest.local.as_ref()) {
                define_assignment_binding(local, *span, visible);
            }
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_boxed_candidates_expr(condition, visible, out);
            collect_boxed_candidates_block(then_branch, &mut visible.clone(), out);
            collect_boxed_candidates_block(else_branch, &mut visible.clone(), out);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_boxed_candidates_expr(condition, visible, out);
            collect_boxed_candidates_block(body, &mut visible.clone(), out);
        }
        Stmt::For {
            name,
            iterable,
            body,
            span,
        } => {
            collect_boxed_candidates_expr(iterable, visible, out);
            let mut scoped = visible.clone();
            // The iterator is created by lower_for rather than a body statement,
            // but still has a stable binding site. Recording it lets lower_for
            // reject closure capture explicitly instead of emitting a closure
            // that reads an unbound C local.
            scoped.insert(name.clone(), Some(BoxedBindingSite::new(name, *span)));
            collect_boxed_candidates_block(body, &mut scoped, out);
        }
        Stmt::Function(function) => {
            record_visible_captures(
                crate::runtime::interpreter::function_capture_names(function),
                visible,
                out,
            );
            // A named local function comes into scope only after its closure is
            // created. Self-recursion is handled by self_recurse, not a cell.
            visible.insert(function.name.clone(), None);
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            finally_body,
            ..
        } => {
            collect_boxed_candidates_block(body, &mut visible.clone(), out);
            let mut catch_visible = visible.clone();
            if let Some(name) = catch_name {
                catch_visible.insert(name.clone(), None);
            }
            collect_boxed_candidates_block(catch_body, &mut catch_visible, out);
            collect_boxed_candidates_block(finally_body, &mut visible.clone(), out);
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
            collect_boxed_candidates_expr(value, visible, out)
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_boxed_candidates_expr(value, visible, out);
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Expr { expr, .. } => collect_boxed_candidates_expr(expr, visible, out),
    }
}

fn collect_boxed_candidates_assign_target(
    target: &AssignTarget,
    visible: &VisibleBoxBindings,
    out: &mut HashSet<BoxedBindingSite>,
) {
    match target {
        AssignTarget::Variable(_) => {}
        AssignTarget::Index { target, index } => {
            collect_boxed_candidates_expr(target, visible, out);
            collect_boxed_candidates_expr(index, visible, out);
        }
        AssignTarget::Field { target, .. } => collect_boxed_candidates_expr(target, visible, out),
    }
}

fn collect_boxed_candidates_expr(
    expr: &Expr,
    visible: &VisibleBoxBindings,
    out: &mut HashSet<BoxedBindingSite>,
) {
    match &expr.kind {
        ExprKind::Function { params, body, .. } => {
            record_visible_captures(
                crate::runtime::interpreter::closure_capture_names(params, body),
                visible,
                out,
            );
        }
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => {
            collect_boxed_candidates_expr(expr, visible, out)
        }
        ExprKind::Binary { left, right, .. } => {
            collect_boxed_candidates_expr(left, visible, out);
            collect_boxed_candidates_expr(right, visible, out);
        }
        ExprKind::Call { callee, args } => {
            collect_boxed_candidates_expr(callee, visible, out);
            for arg in args {
                collect_boxed_candidates_expr(arg, visible, out);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_boxed_candidates_expr(value, visible, out);
            }
        }
        ExprKind::Index { target, index } => {
            collect_boxed_candidates_expr(target, visible, out);
            collect_boxed_candidates_expr(index, visible, out);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_boxed_candidates_expr(target, visible, out)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_boxed_candidates_expr(value, visible, out);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_boxed_candidates_expr(value, visible, out);
            for arm in arms {
                let mut arm_visible = visible.clone();
                bind_non_boxable_pattern_names(&arm.pattern, &mut arm_visible);
                if let Some(guard) = &arm.guard {
                    collect_boxed_candidates_expr(guard, &arm_visible, out);
                }
                collect_boxed_candidates_expr(&arm.value, &arm_visible, out);
            }
        }
        ExprKind::Literal(_) | ExprKind::Variable(_) => {}
    }
}

fn bind_non_boxable_pattern_names(pattern: &MatchPattern, visible: &mut VisibleBoxBindings) {
    match pattern {
        MatchPattern::Binding(name) => {
            visible.insert(name.clone(), None);
        }
        MatchPattern::EnumVariant { fields, .. } => {
            for field in fields {
                bind_non_boxable_pattern_names(field, visible);
            }
        }
        MatchPattern::Wildcard | MatchPattern::Literal(_) => {}
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

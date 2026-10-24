//! Private same-thread fatal return mailbox for synchronous native frames.
//! No allocation, user Result, new deadline, or Task outcome is introduced here.
use std::collections::{HashMap, HashSet, VecDeque};

use crate::ir::BlockId;

use super::{
    c_expr, c_ident, c_symbol, c_zero_value, emit_owned_cleanup, expr_children, is_c_owned_type,
    output::COutput, unsupported, walk_inst_exprs, walk_terminator_exprs, BinaryOp, IrCallKind,
    IrExpr, IrExprKind, IrFunction, IrInst, IrLValue, IrProgram, IrTerminator, IrType, KuResult,
    OwnedLocal, UnaryOp,
};

pub(super) fn checked_helper(value: &IrExpr) -> Option<&'static str> {
    match &value.kind {
        IrExprKind::Unary {
            op: UnaryOp::Negate,
            expr,
        } if expr.ty == IrType::Int => Some("ku_int_neg"),
        IrExprKind::Binary { left, op, right }
            if left.ty == IrType::Int && right.ty == IrType::Int =>
        {
            match op {
                BinaryOp::Add => Some("ku_int_add"),
                BinaryOp::Subtract => Some("ku_int_sub"),
                BinaryOp::Multiply => Some("ku_int_mul"),
                BinaryOp::Divide => Some("ku_int_div"),
                BinaryOp::Remainder => Some("ku_int_rem"),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn uses_checked_integer(program: &IrProgram) -> bool {
    program.functions.iter().any(|function| {
        function.blocks.iter().any(|block| {
            block.instructions.iter().any(|instruction| {
                matches!(instruction, IrInst::Temp { value, .. } if checked_helper(value).is_some())
            })
        })
    })
}

/// Optional fact over already verified IR, never a function-entry assumption.
/// A real SyncGuard empties the TLS mailbox on both explicit successor edges.
/// Only callback-free Copy operations can preserve that fact. A deadline poll,
/// arithmetic operation, call or unknown operation kills it. A successful pure
/// math helper in a proven-empty block can then bypass its redundant TLS read.
#[derive(Default)]
pub(super) struct MathGuardPlan {
    blocks: HashSet<BlockId>,
}

impl MathGuardPlan {
    pub(super) fn new(function: &IrFunction) -> Self {
        Self::try_new(function).unwrap_or_default()
    }

    pub(super) fn contains(&self, block: BlockId) -> bool {
        self.blocks.contains(&block)
    }

    fn try_new(function: &IrFunction) -> Option<Self> {
        // Strictly optional: large/unknown graphs keep the original emitter.
        // Each block can become Empty once; loops never receive an optimistic
        // initial fact. Node/expression/edge visits share this finite budget.
        const MAX_BLOCKS: usize = 16_384;
        let count = function.blocks.len();
        if count == 0 || count > MAX_BLOCKS || function.blocks[0].id != BlockId(0) {
            return None;
        }
        let mut work = 1_000_000usize;
        work = work.checked_sub(count)?;
        work = work.checked_sub(function.params.len())?;
        let index = function
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (b.id, i))
            .collect::<HashMap<_, _>>();
        if index.len() != count {
            return None;
        }
        let mut locals = function
            .params
            .iter()
            .map(|p| (p.name.as_str(), &p.ty))
            .collect::<HashMap<_, _>>();
        for block in &function.blocks {
            for inst in &block.instructions {
                work = work.checked_sub(1)?;
                if let IrInst::Let { name, ty, .. } = inst {
                    if locals.insert(name, ty).is_some_and(|old| old != ty) {
                        return None;
                    }
                }
            }
        }
        let mut predecessors = vec![0usize; count];
        let mut unknown = vec![0usize; count];
        let mut propagate = vec![Vec::new(); count];
        let mut forced_unknown = vec![false; count];
        forced_unknown[0] = true;
        let mut math = vec![false; count];
        for (i, block) in function.blocks.iter().enumerate() {
            work = work.checked_sub(1)?;
            let mut pure_prefix = true;
            for (position, inst) in block.instructions.iter().enumerate() {
                if position + 1 == block.instructions.len()
                    && matches!(&block.terminator, IrTerminator::SyncGuard { .. })
                    && matches!(inst, IrInst::Temp { value, .. } if checked_helper(value).is_some())
                {
                    math[i] = pure_prefix;
                }
                pure_prefix &= preserves_empty_mailbox(inst, &locals, &mut work)?;
            }
            let (targets, constant_empty, transparent) = match &block.terminator {
                IrTerminator::SyncGuard {
                    continue_block,
                    cleanup_block,
                } => (vec![*continue_block, *cleanup_block], true, false),
                IrTerminator::Jump(target) => (vec![*target], false, pure_prefix),
                IrTerminator::Branch {
                    condition,
                    then_block,
                    else_block,
                } => (
                    vec![*then_block, *else_block],
                    false,
                    pure_prefix && pure_copy_expr(condition, &mut work)?,
                ),
                // Polling may publish a deadline failure. Neither edge is an
                // empty-mailbox witness, even with no timeout in a benchmark.
                IrTerminator::Safepoint {
                    continue_block,
                    timeout_block,
                } => (vec![*continue_block, *timeout_block], false, false),
                IrTerminator::ForEach {
                    body_block,
                    after_block,
                    ..
                } => {
                    forced_unknown[*index.get(after_block)?] = true; // implicit element cleanup
                    (vec![*body_block, *after_block], false, false)
                }
                IrTerminator::ResultBranch {
                    ok_block,
                    err_block,
                    ..
                } => (vec![*ok_block, *err_block], false, false),
                IrTerminator::JumpErr { target, .. } => {
                    let mut targets = vec![*target];
                    if let Some(next) = function.blocks.get(i + 1) {
                        targets.push(next.id);
                    }
                    (targets, false, false)
                }
                IrTerminator::Next => (
                    function
                        .blocks
                        .get(i + 1)
                        .map(|next| vec![next.id])
                        .unwrap_or_default(),
                    false,
                    false,
                ),
                IrTerminator::Return(_)
                | IrTerminator::PropagateErr(_)
                | IrTerminator::Unreachable => (Vec::new(), false, false),
            };
            for target in targets {
                work = work.checked_sub(1)?;
                let target = *index.get(&target)?;
                predecessors[target] += 1;
                if !constant_empty {
                    unknown[target] += 1;
                    if transparent {
                        propagate[i].push(target);
                    }
                }
            }
        }
        let mut queue = (0..count)
            .filter(|&i| predecessors[i] != 0 && unknown[i] == 0 && !forced_unknown[i])
            .collect::<VecDeque<_>>();
        let mut empty = vec![false; count];
        while let Some(i) = queue.pop_front() {
            work = work.checked_sub(1)?;
            if empty[i] || forced_unknown[i] {
                continue;
            }
            empty[i] = true;
            for &target in &propagate[i] {
                work = work.checked_sub(1)?;
                unknown[target] = unknown[target].checked_sub(1)?;
                if unknown[target] == 0 && !forced_unknown[target] {
                    queue.push_back(target);
                }
            }
        }
        Some(Self {
            blocks: function
                .blocks
                .iter()
                .enumerate()
                .filter_map(|(i, block)| (math[i] && empty[i]).then_some(block.id))
                .collect(),
        })
    }
}

fn copy_scalar(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null
    )
}

fn pure_copy_expr(expr: &IrExpr, work: &mut usize) -> Option<bool> {
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        *work = work.checked_sub(1)?;
        if !copy_scalar(&expr.ty) {
            return Some(false);
        }
        match &expr.kind {
            IrExprKind::Literal(_)
            | IrExprKind::Local(_)
            | IrExprKind::Temp(_)
            | IrExprKind::BorrowedParam(_)
            | IrExprKind::BorrowedTemp(_) => {}
            IrExprKind::Unary {
                op: UnaryOp::Not,
                expr,
            } => pending.push(expr),
            IrExprKind::Binary { left, op, right }
                if matches!(
                    op,
                    BinaryOp::Equal
                        | BinaryOp::NotEqual
                        | BinaryOp::Less
                        | BinaryOp::LessEqual
                        | BinaryOp::Greater
                        | BinaryOp::GreaterEqual
                        | BinaryOp::And
                        | BinaryOp::Or
                ) =>
            {
                pending.push(left);
                pending.push(right);
            }
            _ => return Some(false),
        }
    }
    Some(true)
}

fn preserves_empty_mailbox(
    inst: &IrInst,
    locals: &HashMap<&str, &IrType>,
    work: &mut usize,
) -> Option<bool> {
    *work = work.checked_sub(1)?;
    match inst {
        IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } if copy_scalar(ty) => {
            pure_copy_expr(value, work)
        }
        IrInst::Store {
            target: IrLValue::Local(name),
            value,
        } if locals
            .get(name.as_str())
            .is_some_and(|ty| copy_scalar(ty) && **ty == value.ty) =>
        {
            pure_copy_expr(value, work)
        }
        IrInst::Expr(value) => pure_copy_expr(value, work),
        _ => Some(false),
    }
}

fn validate_expr(expr: &IrExpr, allow_guarded_root: bool) -> KuResult<()> {
    if (checked_helper(expr).is_some() || crate::ir::ir_expr_needs_sync_guard(expr))
        && !allow_guarded_root
    {
        return Err(unsupported(
            "native C synchronous arithmetic/call requires a normalized immediate SyncGuard",
        ));
    }
    // Only the one top-level instruction may own the adjacent guard. Nested
    // work, including a call under a borrow/field/return, must be materialized.
    for child in expr_children(expr) {
        validate_expr(child, false)?;
    }
    Ok(())
}

fn validate_place(place: &IrLValue) -> KuResult<()> {
    match place {
        IrLValue::Local(_) => Ok(()),
        IrLValue::Index { target, index } => {
            validate_expr(target, false)?;
            validate_expr(index, false)
        }
        IrLValue::Field { target, .. } => validate_expr(target, false),
    }
}

pub(super) fn validate_program(program: &IrProgram) -> KuResult<()> {
    for function in &program.functions {
        for block in &function.blocks {
            for (index, instruction) in block.instructions.iter().enumerate() {
                let guarded = index + 1 == block.instructions.len()
                    && matches!(block.terminator, IrTerminator::SyncGuard { .. });
                let guarded_root =
                    guarded && matches!(instruction, IrInst::Temp { .. } | IrInst::Expr(_));
                let mut result = Ok(());
                walk_inst_exprs(instruction, &mut |expr| {
                    if result.is_ok() {
                        result = validate_expr(expr, guarded_root);
                    }
                });
                result?;
                // These payloads are not part of walk_inst_exprs' type walk.
                match instruction {
                    IrInst::Store { target, .. } => validate_place(target)?,
                    IrInst::BindError { result, .. } => validate_expr(result, false)?,
                    _ => {}
                }
                if let IrInst::Temp { ty, value, .. } = instruction {
                    if checked_helper(value).is_some()
                        && (*ty != IrType::Int || value.ty != IrType::Int)
                    {
                        return Err(unsupported("checked integer destination must be int"));
                    }
                }
            }
            let mut result = Ok(());
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                if result.is_ok() {
                    result = validate_expr(expr, false);
                }
            });
            result?;
        }
    }
    Ok(())
}

// Only ordinary C identifiers emitted by this feature are reserved here.
// Source fields/capture members and the C label namespace do not participate.
const SYNC_GLOBAL_NAMES: &[&str] = &[
    "KuSyncExitSignal",
    "KU_SYNC_EXIT_NONE",
    "KU_SYNC_EXIT_ARITHMETIC_FATAL",
    "KU_SYNC_EXIT_HTTP_JSON_FATAL",
    "KU_SYNC_EXIT_CLEANUP_ABORT",
    "__ku_sync_return_signal",
    "__ku_sync_reset",
    "__ku_sync_take",
    "__ku_sync_publish",
    "__ku_sync_raise_math",
    "__ku_sync_raise_http_json",
    "__ku_sync_error_message",
    "__ku_sync_error_message_length",
    "__ku_sync_finish_request",
];
const INTEGER_GLOBAL_NAMES: &[&str] = &[
    "ku_int_neg",
    "ku_int_add",
    "ku_int_sub",
    "ku_int_mul",
    "ku_int_div",
    "ku_int_rem",
    "KU_INT_OK",
    "KU_INT_OVERFLOW",
    "KU_INT_DIV_ZERO",
    "KuSyncStatusContract",
];

fn identifier_collision(name: &str) -> crate::error::KuError {
    unsupported(format!(
        "native C synchronous internal identifier '{name}' conflicts with a user binding"
    ))
}

fn require_unhidden(visible: &HashSet<String>, names: &[&str]) -> KuResult<()> {
    for name in names {
        if visible.contains(*name) {
            return Err(identifier_collision(name));
        }
    }
    Ok(())
}

fn add_frame_binding(visible: &mut HashSet<String>, name: &str) -> KuResult<()> {
    let name = c_ident(name);
    if name == "__ku_sync_deferred" {
        return Err(identifier_collision(&name));
    }
    visible.insert(name);
    Ok(())
}

fn references_ordinary_name(expr: &IrExpr, name: &str) -> bool {
    if matches!(&expr.kind, IrExprKind::Local(value) | IrExprKind::BorrowedParam(value)
        if c_symbol(value) == name)
    {
        return true;
    }
    expr_children(expr)
        .into_iter()
        .any(|child| references_ordinary_name(child, name))
}

fn validate_direct_callee(expr: &IrExpr) -> KuResult<()> {
    if let IrExprKind::Call {
        callee,
        kind: IrCallKind::Direct(_),
        ..
    } = &expr.kind
    {
        // This per-frame local masks a same-named global function. Thunks live
        // in a different C scope and do not have the local, so they are allowed.
        if references_ordinary_name(callee, "__ku_sync_deferred") {
            return Err(identifier_collision("__ku_sync_deferred"));
        }
    }
    for child in expr_children(expr) {
        validate_direct_callee(child)?;
    }
    Ok(())
}

pub(super) fn validate_identifiers(
    program: &IrProgram,
    checked_integer_runtime: bool,
) -> KuResult<()> {
    for function in &program.functions {
        let name = c_symbol(&function.name);
        if SYNC_GLOBAL_NAMES.contains(&name.as_str())
            || checked_integer_runtime && INTEGER_GLOBAL_NAMES.contains(&name.as_str())
        {
            return Err(identifier_collision(&name));
        }
        let mut visible = HashSet::new();
        for param in &function.params {
            add_frame_binding(&mut visible, &param.name)?;
        }
        // The frame signal declaration precedes hoisted owners and loop locals.
        require_unhidden(&visible, &["KuSyncExitSignal"])?;
        for block in &function.blocks {
            for instruction in &block.instructions {
                match instruction {
                    IrInst::Let { name, ty, .. } if is_c_owned_type(ty) => {
                        add_frame_binding(&mut visible, name)?;
                    }
                    IrInst::CellNew { name, .. } | IrInst::BindError { name, .. } => {
                        add_frame_binding(&mut visible, name)?;
                    }
                    _ => {}
                }
            }
            if let IrTerminator::ForEach { name, .. } = &block.terminator {
                add_frame_binding(&mut visible, name)?;
            }
        }
        // C labels do not create scopes. Track Copy declarations in emitted
        // block order; a declaration after the last guard need not hide its type.
        for block in &function.blocks {
            for instruction in &block.instructions {
                if let IrInst::Let { name, ty, .. } = instruction {
                    if !is_c_owned_type(ty) {
                        add_frame_binding(&mut visible, name)?;
                    }
                }
                if let IrInst::Temp { value, .. } = instruction {
                    if let Some(helper) = checked_helper(value) {
                        require_unhidden(&visible, &[helper, "__ku_sync_raise_math"])?;
                        // A C declarator enters scope before its initializer.
                        // Reject only operands actually evaluated inside this
                        // scratch declaration, not harmless uses in other code.
                        if references_ordinary_name(value, "__ku_status") {
                            return Err(identifier_collision("__ku_status"));
                        }
                    }
                }
                if matches!(instruction, IrInst::Fail(_)) {
                    require_unhidden(&visible, &["KU_SYNC_EXIT_NONE"])?;
                }
                let mut result = Ok(());
                walk_inst_exprs(instruction, &mut |expr| {
                    if result.is_ok() {
                        result = validate_direct_callee(expr);
                    }
                });
                result?;
                match instruction {
                    IrInst::Store {
                        target: IrLValue::Index { target, index },
                        ..
                    } => {
                        validate_direct_callee(target)?;
                        validate_direct_callee(index)?;
                    }
                    IrInst::Store {
                        target: IrLValue::Field { target, .. },
                        ..
                    } => {
                        validate_direct_callee(target)?;
                    }
                    IrInst::BindError { result, .. } => validate_direct_callee(result)?,
                    _ => {}
                }
            }
            match block.terminator {
                IrTerminator::SyncGuard { .. } => require_unhidden(
                    &visible,
                    &[
                        "KuSyncExitSignal",
                        "KU_SYNC_EXIT_NONE",
                        "KU_SYNC_EXIT_ARITHMETIC_FATAL",
                        "KU_SYNC_EXIT_HTTP_JSON_FATAL",
                        "__ku_sync_take",
                    ],
                )?,
                IrTerminator::Return(_) | IrTerminator::PropagateErr(_) => {
                    require_unhidden(&visible, &["KU_SYNC_EXIT_NONE"])?;
                }
                _ => {}
            }
            let mut result = Ok(());
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                if result.is_ok() {
                    result = validate_direct_callee(expr);
                }
            });
            result?;
        }
        if function.return_type == IrType::Void {
            require_unhidden(&visible, &["KU_SYNC_EXIT_NONE"])?;
        }
        require_unhidden(&visible, &["__ku_sync_publish"])?;
    }
    Ok(())
}

pub(super) fn emit_math_temp(
    out: &mut COutput,
    id: crate::ir::TempId,
    value: &IrExpr,
) -> KuResult<bool> {
    out.check()?;
    let Some(helper) = checked_helper(value) else {
        return Ok(false);
    };
    let arguments = match &value.kind {
        IrExprKind::Unary { expr, .. } => c_expr(expr)?,
        IrExprKind::Binary { left, right, .. } => {
            format!("{}, {}", c_expr(left)?, c_expr(right)?)
        }
        _ => unreachable!("checked_helper selected a scalar math expression"),
    };
    // Failure does not write tN; its adjacent SyncGuard prevents all reads and
    // source assignments. Only the helper, not a raw C operator, computes it.
    out.push_str(&format!(
        "  int64_t t{0};\n  {{ uint32_t __ku_status = {helper}({arguments}, &t{0}); __ku_sync_raise_math(__ku_status); }}\n",
        id.0,
    ));
    out.check()?;
    Ok(true)
}

/// Open a private status scope; emit_block closes it after the unchanged
/// SyncGuard. Only a MathGuardPlan witness may use the success shortcut. A
/// failure still publishes and consumes the original first-error mailbox.
pub(super) fn emit_fast_math_temp(
    out: &mut COutput,
    id: crate::ir::TempId,
    value: &IrExpr,
    continue_block: BlockId,
) -> KuResult<()> {
    let helper = checked_helper(value).ok_or_else(|| unsupported("missing checked math helper"))?;
    let arguments = match &value.kind {
        IrExprKind::Unary { expr, .. } => c_expr(expr)?,
        IrExprKind::Binary { left, right, .. } => format!("{}, {}", c_expr(left)?, c_expr(right)?),
        _ => return Err(unsupported("invalid checked math expression")),
    };
    out.push_str(&format!(
        "  int64_t t{0};\n  {{ uint32_t __ku_status = {helper}({arguments}, &t{0}); __ku_sync_raise_math(__ku_status);\n  if (__ku_status == KU_INT_OK) goto block{1};\n",
        id.0, continue_block.0,
    ));
    out.check()
}

pub(super) fn emit_return_guard(out: &mut COutput) -> KuResult<()> {
    out.check()?;
    out.push_str("  if (__ku_sync_deferred.kind != KU_SYNC_EXIT_NONE) goto __ku_sync_epilogue;\n");
    out.check()
}

pub(super) fn emit_epilogue(
    out: &mut COutput,
    function: &IrFunction,
    owned: &[OwnedLocal],
) -> KuResult<()> {
    out.check()?;
    out.push_str("__ku_sync_epilogue:;\n");
    emit_owned_cleanup(out, owned)?;
    out.push_str("  if (__ku_timeout_unwind) __ku_handler_timeout_leave();\n  __ku_call_depth--;\n  __ku_sync_publish(__ku_sync_deferred);\n");
    if function.return_type == IrType::Void {
        out.push_str("  return;\n");
    } else {
        out.push_str(&format!(
            "  return {};\n",
            c_zero_value(&function.return_type)?
        ));
    }
    out.check()
}

pub(super) fn emit_runtime(out: &mut COutput) -> KuResult<()> {
    out.check()?;
    out.push_str(SYNC_RUNTIME);
    out.check()
}

const SYNC_RUNTIME: &str = r#"
/* A return mailbox, not a persistent cancellation flag. Every generated call
 * takes it immediately; only execution roots reset it. No owning payload. */
enum { KU_SYNC_EXIT_NONE = 0u, KU_SYNC_EXIT_ARITHMETIC_FATAL = 1u,
       KU_SYNC_EXIT_CLEANUP_ABORT = 2u, KU_SYNC_EXIT_HTTP_JSON_FATAL = 3u };
typedef struct KuSyncExitSignal {
  uint8_t kind;
  uint8_t arithmetic_status;
  const uint8_t* static_message;
  size_t static_message_len;
} KuSyncExitSignal;
static KU_THREAD_LOCAL KuSyncExitSignal __ku_sync_return_signal;
static void __ku_sync_reset(void) { __ku_sync_return_signal = (KuSyncExitSignal){0}; }
static KuSyncExitSignal __ku_sync_take(void) {
  /* Only reset/take or a nonempty publish write this mailbox. */
  if (__ku_sync_return_signal.kind == KU_SYNC_EXIT_NONE)
    return (KuSyncExitSignal){0};
  KuSyncExitSignal signal = __ku_sync_return_signal;
  __ku_sync_return_signal = (KuSyncExitSignal){0};
  return signal;
}
static void __ku_sync_publish(KuSyncExitSignal signal) {
  if (signal.kind != KU_SYNC_EXIT_NONE && __ku_sync_return_signal.kind == KU_SYNC_EXIT_NONE)
    __ku_sync_return_signal = signal;
}
static void __ku_sync_raise_math(uint32_t status) {
  /* These are the existing pure arithmetic helper success/error statuses.
   * The mailbox is emitted even when comparison-only code needs no helpers. */
  if (status == 0u) return;
  KuSyncExitSignal signal = {0};
  signal.kind = (__ku_handler_timed_out && __ku_handler_unwind_depth != 0)
      ? KU_SYNC_EXIT_CLEANUP_ABORT : KU_SYNC_EXIT_ARITHMETIC_FATAL;
  signal.arithmetic_status = (uint8_t)status;
  __ku_sync_publish(signal);
}
/* Callers may only supply a proven static view. No owner crosses the mailbox. */
static void __ku_sync_raise_http_json(const uint8_t* message, size_t len) {
  KuSyncExitSignal signal = {0};
  signal.kind = KU_SYNC_EXIT_HTTP_JSON_FATAL;
  signal.static_message = message;
  signal.static_message_len = len;
  __ku_sync_publish(signal);
}
static const char* __ku_sync_error_message(KuSyncExitSignal signal) {
  if (signal.kind == KU_SYNC_EXIT_HTTP_JSON_FATAL && signal.static_message)
    return (const char*)signal.static_message;
  return signal.arithmetic_status == 2u ? "division by zero" : "integer overflow";
}
static size_t __ku_sync_error_message_length(KuSyncExitSignal signal) {
  if (signal.kind == KU_SYNC_EXIT_HTTP_JSON_FATAL && signal.static_message)
    return signal.static_message_len;
  return strlen(__ku_sync_error_message(signal));
}
static int __ku_sync_finish_request(KuSyncExitSignal* signal) {
  *signal = __ku_sync_take();
  int selected_timeout = __ku_handler_timed_out;
  int polled_timeout = __ku_handler_timeout_finish();
  /* A math failure is classified at its operation/guard, not by a later root
   * clock sample. Already-selected timeout still owns the response as 504. */
  return signal->kind == KU_SYNC_EXIT_NONE ? polled_timeout : selected_timeout;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(source: &str) -> IrProgram {
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
            .parse_program()
            .unwrap();
        crate::checker::Checker::new().check(&ast).unwrap();
        crate::ir::lower_program(&ast).unwrap()
    }

    #[test]
    fn native_sync_empty_mailbox_facts_require_all_incoming_guards() {
        let program = lower(
            "fn Chain(a:int,d:int):int { first=a+1 second=first+d return second*3 } fn main() {}",
        );
        let function = &program.functions[0];
        let math = function.blocks.iter().filter(|block| matches!(block.instructions.last(), Some(IrInst::Temp { value, .. }) if checked_helper(value).is_some())).collect::<Vec<_>>();
        assert_eq!(math.len(), 3);
        let plan = MathGuardPlan::new(function);
        assert!(
            !plan.contains(math[0].id),
            "function entry may have a pending error"
        );
        assert!(plan.contains(math[1].id) && plan.contains(math[2].id));

        let mixed = lower("fn Choose(a:int,flag:bool):int { if(flag) { println(a) } else { value=a+1 } return a+2 } fn main() {}");
        let function = &mixed.functions[0];
        let last = function.blocks.iter().find(|block| matches!(block.instructions.last(), Some(IrInst::Temp { value: IrExpr { kind: IrExprKind::Binary { right, .. }, .. }, .. }) if matches!(&right.kind, IrExprKind::Literal(v) if v=="2"))).unwrap();
        assert!(
            !MathGuardPlan::new(function).contains(last.id),
            "one unproven branch must defeat the merge"
        );

        let loops =
            lower("fn Loop(a:int,n:int):int { i=0 while(i<n) { i=i+1 } return a+2 } fn main() {}");
        let function = &loops.functions[0];
        assert!(
            MathGuardPlan::new(function).blocks.is_empty(),
            "a deadline poll never proves an empty mailbox"
        );

        // Even malformed external IR must not let a successful final operation
        // hide an earlier producer in the same block. Public validation rejects
        // this layout too, before it can reach C generation.
        let mut invalid = program.functions[0].clone();
        let earlier = math[0].instructions.last().unwrap().clone();
        let target = math[1].id;
        invalid
            .blocks
            .iter_mut()
            .find(|block| block.id == target)
            .unwrap()
            .instructions
            .insert(0, earlier);
        assert!(!MathGuardPlan::new(&invalid).contains(target));
    }

    #[test]
    fn native_sync_identifier_source_rejects_only_actual_generated_collisions() {
        let cases = [
            ("fn KuSyncExitSignal(): int { return 7 } fn main() {}", "KuSyncExitSignal"),
            ("fn KU_SYNC_EXIT_NONE(): int { return 7 } fn main() {}", "KU_SYNC_EXIT_NONE"),
            ("fn ku_int_add(): int { return 7 } fn Calc(a: int): int { return a + 1 } fn main() {}", "ku_int_add"),
            ("fn Calc(ku_int_add: int): int { return ku_int_add + 1 } fn main() {}", "ku_int_add"),
            ("fn Calc(KuSyncExitSignal: int): int { return KuSyncExitSignal } fn main() {}", "KuSyncExitSignal"),
            ("fn Calc(KU_SYNC_EXIT_NONE: int): int { return KU_SYNC_EXIT_NONE } fn main() {}", "KU_SYNC_EXIT_NONE"),
            ("fn Calc(KU_SYNC_EXIT_ARITHMETIC_FATAL: int): int { return KU_SYNC_EXIT_ARITHMETIC_FATAL + 1 } fn main() {}", "KU_SYNC_EXIT_ARITHMETIC_FATAL"),
        ];
        for (source, name) in cases {
            // All these names are legal source, unlike the already reserved
            // __ku_ source prefix. The actual C emitter must fail closed.
            let program = lower(source);
            let error = super::super::generate_c_source(&program).unwrap_err();
            assert!(
                error
                    .message
                    .contains(&format!("internal identifier '{name}'")),
                "{error}"
            );
        }
        for source in [
            "fn result(): int { return 7 } fn signal(): int { return 8 } fn main() { println(result()) println(signal()) }",
            "fn ku_int_add(): int { return 7 } fn main() { println(ku_int_add()) }",
            "fn Calc(ku_int_sub: int): int { return ku_int_sub + 1 } fn main() {}",
            "fn Calc(KU_SYNC_EXIT_CLEANUP_ABORT: int): int { return KU_SYNC_EXIT_CLEANUP_ABORT + 1 } fn main() {}",
            "fn Calc(a: int): int { value = a + 1 KuSyncExitSignal = 7 return value } fn main() {}",
            "struct KuSyncExitSignal { KU_SYNC_EXIT_NONE: int } fn Calc(value: KuSyncExitSignal): int { return value.KU_SYNC_EXIT_NONE + 1 } fn main() {}",
        ] {
            super::super::generate_c_source(&lower(source)).unwrap();
        }
    }

    #[test]
    fn native_sync_identifier_ir_boundary_checks_mapped_names_and_emission_presence() {
        let reference = lower("fn Helper(): int { return 7 } fn main() {}");
        for name in SYNC_GLOBAL_NAMES.iter().chain(INTEGER_GLOBAL_NAMES.iter()) {
            let mut program = reference.clone();
            program.functions[0].name = (*name).to_owned();
            let error = validate_identifiers(&program, true).unwrap_err();
            assert!(error.message.contains(*name), "{error}");
            if INTEGER_GLOBAL_NAMES.contains(name) {
                validate_identifiers(&program, false).unwrap();
            }
        }
        let mut mapped = reference.clone();
        mapped.functions[0].name = "ku.int.add".to_owned();
        assert!(validate_identifiers(&mapped, true)
            .unwrap_err()
            .message
            .contains("ku_int_add"));
        validate_identifiers(&mapped, false).unwrap();
        for harmless in [
            "__ku_sync_deferred",
            "__ku_observed",
            "__ku_status",
            "__ku_other",
        ] {
            let mut program = reference.clone();
            program.functions[0].name = harmless.to_owned();
            validate_identifiers(&program, false).unwrap();
        }
        // The public frame emitter must also account for helpers requested only
        // by the Task half of a mixed artifact, not just synchronous arithmetic.
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(
            "async fn Worker(a: int): int! { return ok(a + 1) } async fn main(): null! { value = (await Worker(2))? println(value) return ok(null) }",
        ).tokenize().unwrap()).parse_program().unwrap();
        crate::checker::Checker::new().check(&ast).unwrap();
        let native = crate::ir::task_lower::lower_program(&ast).unwrap();
        // ku_int_* already has an earlier Task ABI guard. Use this new shared
        // status typedef to exercise the actual emission-presence check here.
        let sync = lower("fn KuSyncStatusContract(): int { return 7 } fn main() {}");
        super::super::generate_c_source(&sync).unwrap();
        let error = super::super::generate_task_frame_c_source(&sync, &native.tasks).unwrap_err();
        assert!(error.message.contains("KuSyncStatusContract"), "{error}");
    }

    #[test]
    fn native_sync_identifier_ir_boundary_checks_real_scratch_scopes() {
        // Checker already rejects __ku_ source names. These are explicit IR
        // boundary tests after lowering valid source, not new source syntax.
        let reference = lower("fn Calc(a: int): int { return a + 1 } fn main() {}");
        for (name, rejected) in [
            ("__ku_status", true),
            ("__ku_sync_deferred", true),
            ("__ku_sync_raise_math", true),
            ("__ku_sync_take", true),
            ("__ku_sync_publish", true),
            ("__ku_observed", false),
            ("__ku_other", false),
        ] {
            let mut program = reference.clone();
            program.functions[0].params[0].name = name.to_owned();
            let value = program.functions[0]
                .blocks
                .iter_mut()
                .flat_map(|block| &mut block.instructions)
                .find_map(|inst| match inst {
                    IrInst::Temp { value, .. } if checked_helper(value).is_some() => Some(value),
                    _ => None,
                })
                .unwrap();
            let IrExprKind::Binary { left, .. } = &mut value.kind else {
                unreachable!()
            };
            assert!(matches!(&left.kind, IrExprKind::Local(original) if original == "a"));
            left.kind = IrExprKind::Local(name.to_owned());
            let result = super::super::generate_c_source(&program);
            if rejected {
                assert!(result.unwrap_err().message.contains(name));
            } else {
                result.unwrap();
            }
        }
        // An unused __ku_status param is outside the new initializer scope.
        let mut unused = lower("fn Calc(a: int): int { return 7 } fn main() {}");
        unused.functions[0].params[0].name = "__ku_status".to_owned();
        super::super::generate_c_source(&unused).unwrap();
        let mut direct = lower("fn Helper(): int { return 7 } fn main() { println(Helper()) }");
        direct.functions[0].name = "__ku_sync_deferred".to_owned();
        let mut changed = 0;
        for function in &mut direct.functions {
            for instruction in function
                .blocks
                .iter_mut()
                .flat_map(|block| &mut block.instructions)
            {
                let IrInst::Temp { value, .. } = instruction else {
                    continue;
                };
                if let IrExprKind::Call {
                    callee,
                    kind: IrCallKind::Direct(_),
                    ..
                } = &mut value.kind
                {
                    if matches!(&callee.kind, IrExprKind::Local(name) if name == "Helper") {
                        callee.kind = IrExprKind::Local("__ku_sync_deferred".to_owned());
                        changed += 1;
                    }
                }
            }
        }
        assert_eq!(changed, 1);
        assert!(super::super::generate_c_source(&direct)
            .unwrap_err()
            .message
            .contains("__ku_sync_deferred"));
        let ordinary = IrExpr {
            ty: IrType::Int,
            kind: IrExprKind::Local("__ku_status".to_owned()),
        };
        let captured = IrExpr {
            ty: IrType::Cell(Box::new(IrType::Int)),
            kind: IrExprKind::CapturedCell("__ku_status".to_owned()),
        };
        let member = IrExpr {
            ty: IrType::Int,
            kind: IrExprKind::Field {
                target: Box::new(IrExpr {
                    ty: IrType::Named("Payload".to_owned()),
                    kind: IrExprKind::Local("payload".to_owned()),
                }),
                name: "__ku_status".to_owned(),
            },
        };
        assert!(references_ordinary_name(&ordinary, "__ku_status"));
        assert!(!references_ordinary_name(&captured, "__ku_status"));
        assert!(!references_ordinary_name(&member, "__ku_status"));
        let cell = IrExpr {
            ty: IrType::Int,
            kind: IrExprKind::CellLoad(Box::new(captured.clone())),
        };
        assert!(!references_ordinary_name(&cell, "__ku_status"));
        let cell = IrExpr {
            ty: IrType::Int,
            kind: IrExprKind::CellLoad(Box::new(IrExpr {
                ty: IrType::Cell(Box::new(IrType::Int)),
                kind: IrExprKind::Local("__ku_status".to_owned()),
            })),
        };
        assert!(references_ordinary_name(&cell, "__ku_status"));
    }

    #[test]
    fn native_sync_runtime_output_limit_is_exact_and_sticky() {
        let mut reference = COutput::new(16 * 1024);
        emit_runtime(&mut reference).unwrap();
        let reference = reference.finish().unwrap();
        let mut exact = COutput::new(reference.len());
        emit_runtime(&mut exact).unwrap();
        assert_eq!(exact.finish().unwrap(), reference);
        for limit in [0, reference.len() / 2, reference.len() - 1] {
            let mut output = COutput::new(limit);
            let error = emit_runtime(&mut output).unwrap_err();
            assert!(error.message.contains("native C output limit exceeded"));
            assert_eq!(emit_runtime(&mut output).unwrap_err(), error);
            assert_eq!(output.finish().unwrap_err(), error);
        }
    }

    #[test]
    fn native_sync_rejects_unguarded_nested_and_mistyped_integer_ir() {
        let source = "fn Calc(a: int, b: int): int { return a + b } fn main() {}";
        let reference = lower(source);
        let block_index = reference.functions[0]
            .blocks
            .iter()
            .position(|block| {
                block.instructions.iter().any(|instruction| {
                matches!(instruction, IrInst::Temp { value, .. } if checked_helper(value).is_some())
            })
            })
            .unwrap();
        let mut unguarded = reference.clone();
        let block = &mut unguarded.functions[0].blocks[block_index];
        let IrTerminator::SyncGuard { continue_block, .. } = block.terminator else {
            panic!("actual source operation must have its immediate guard")
        };
        block.terminator = IrTerminator::Jump(continue_block);
        assert!(validate_program(&unguarded)
            .unwrap_err()
            .message
            .contains("SyncGuard"));
        assert!(super::super::generate_c_source(&unguarded).is_err());
        for wrong_type in [false, true] {
            let mut program = reference.clone();
            let value = program.functions[0].blocks[block_index]
                .instructions
                .iter_mut()
                .find_map(|instruction| match instruction {
                    IrInst::Temp { value, .. } if checked_helper(value).is_some() => Some(value),
                    _ => None,
                })
                .unwrap();
            if wrong_type {
                value.ty = IrType::Float;
            } else {
                *value = IrExpr {
                    kind: IrExprKind::Unary {
                        op: UnaryOp::Negate,
                        expr: Box::new(value.clone()),
                    },
                    ty: IrType::Int,
                };
            }
            assert!(validate_program(&program).is_err());
            assert!(super::super::generate_c_source(&program).is_err());
        }
    }

    #[test]
    fn native_sync_and_task_share_exactly_one_checked_helper_block() {
        let comparison = lower("fn Compare(a: int, b: int): bool { return a < b } fn main() {}");
        let generated = super::super::generate_c_source(&comparison).unwrap();
        assert!(!generated.contains("static uint32_t ku_int_"));
        let sync = lower("fn Calc(a: int, b: int): int { return a + b } fn main() {}");
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(
            "async fn Worker(a: int, b: int): int! { return ok(a * b) } async fn main(): null! { value = (await Worker(2, 3))? println(value) return ok(null) }",
        ).tokenize().unwrap()).parse_program().unwrap();
        crate::checker::Checker::new().check(&ast).unwrap();
        let native = crate::ir::task_lower::lower_program(&ast).unwrap();
        let generated = super::super::generate_task_frame_c_source(&sync, &native.tasks).unwrap();
        for helper in ["neg", "add", "sub", "mul", "div", "rem"] {
            assert_eq!(
                generated
                    .matches(&format!("static uint32_t ku_int_{helper}("))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn native_sync_private_zero_fastpath_preserves_unsupported_type_rejection() {
        // Start from checked source, then inject deliberately malformed raw IR.
        // The marker is compiler-private; this is not a legal source program.
        let reference = lower("fn Value(): int { return 7 } fn main() {}");
        for ty in [IrType::Unknown, IrType::Function, IrType::Void] {
            let mut program = reference.clone();
            let function = program
                .functions
                .iter_mut()
                .find(|function| function.name == "Value")
                .unwrap();
            function.blocks[0].terminator = IrTerminator::Return(Some(IrExpr {
                kind: IrExprKind::Literal("<native-zero>".into()),
                ty,
            }));
            let error = super::super::generate_c_source(&program).unwrap_err();
            assert!(
                error
                    .message
                    .contains("does not support zero initialization"),
                "{error}"
            );
        }

        // Supported Copy and Owned synthetic zeros still share the same exit;
        // the validation above must not recreate a separate return expression.
        for source in [
            "fn Value(): int { return 7 } fn main() {}",
            "fn Value(): str { return \"held\" } fn main() {}",
        ] {
            let mut program = lower(source);
            let function = program
                .functions
                .iter_mut()
                .find(|function| function.name == "Value")
                .unwrap();
            function.blocks[0].terminator = IrTerminator::Return(Some(IrExpr {
                kind: IrExprKind::Literal("<native-zero>".into()),
                ty: function.return_type.clone(),
            }));
            let generated = super::super::generate_c_source(&program).unwrap();
            assert!(generated.contains("  goto __ku_sync_epilogue;\n"));
        }

        // A source string spelling the tag is ordinary owned data, not an
        // internal zero. Its surrounding quotes survive source lowering.
        let program = lower("fn Value(): str { return \"<native-zero>\" } fn main() {}");
        let generated = super::super::generate_c_source(&program).unwrap();
        assert!(generated.contains("KuString __ku_return ="));
        assert!(generated.contains("\"<native-zero>\", 13"));
    }

    #[test]
    fn native_sync_artifact_caps_include_mailbox_guard_and_owner_epilogue() {
        let program = lower("fn Calc(a: int, b: int): int { owner = \"held-\" + \"owner\" return a / b } fn main() { println(Calc(7, 1)) }");
        let options = super::super::CBackendOptions::default();
        let expected = super::super::generate_c_source(&program).unwrap();
        let bounded = |limit| super::super::generate_c_source_bounded(&program, &options, limit);
        assert_eq!(bounded(expected.len()).unwrap(), expected);
        let mailbox = expected.find("typedef struct KuSyncExitSignal").unwrap() + 8;
        let guard = expected.find("{ KuSyncExitSignal __ku_observed").unwrap() + 8;
        let epilogue = expected.find("__ku_sync_epilogue:;").unwrap() + 8;
        for limit in [mailbox, guard, epilogue, expected.len() - 1] {
            let error = bounded(limit).unwrap_err();
            assert!(
                error.message.contains("native C output limit exceeded"),
                "{error}"
            );
        }
    }
}

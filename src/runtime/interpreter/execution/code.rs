//! Owned, flat syntax for the admitted synchronous scalar slice.
//!
//! AST references exist only in the bounded builder. Arena edges are numeric
//! IDs, so neither recursive syntax cloning nor recursive arena destruction is
//! needed. Captures and user values never enter this shared code owner.

use super::*;
use std::sync::Arc;

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub(super) struct $name(pub(super) usize);
    };
}
id!(FnId);
id!(StmtId);
id!(ExprId);
id!(BlockId);
id!(SpanId);
id!(SymbolId);

#[derive(Clone, Copy, Default)]
pub(super) struct ExprRange {
    start: usize,
    pub(super) len: usize,
}

impl ExprRange {
    pub(super) fn get(self, next: usize) -> Option<ExprId> {
        (next < self.len).then(|| ExprId(self.start + next))
    }
}

#[derive(Clone, Copy, Default)]
struct ParamRange {
    start: usize,
    len: usize,
}

#[derive(Clone, Copy, Default)]
pub(super) struct CodeParam {
    pub(super) name: SymbolId,
    pub(super) span: SpanId,
}

#[derive(Clone, Copy, Default)]
pub(super) struct CodeFn {
    pub(super) name: SymbolId,
    params: ParamRange,
    pub(super) body: BlockId,
    pub(super) span: SpanId,
}

impl CodeFn {
    pub(super) fn arity(self) -> usize {
        self.params.len
    }
}

#[derive(Clone, Copy, Default)]
struct CodeBlock {
    start: usize,
    len: usize,
    span: SpanId,
}

#[derive(Clone, Copy)]
pub(super) enum ScalarLiteral {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

#[derive(Clone, Copy)]
pub(super) enum ExprCode {
    Literal(ScalarLiteral),
    Variable(SymbolId),
    Unary(UnaryOp, ExprId),
    Binary(ExprId, BinaryOp, ExprId),
    Call {
        function: FnId,
        args: ExprRange,
        callee_span: SpanId,
    },
}

#[derive(Clone, Copy)]
pub(super) struct CodeExpr {
    pub(super) kind: ExprCode,
    pub(super) span: SpanId,
}

impl Default for CodeExpr {
    fn default() -> Self {
        Self {
            kind: ExprCode::Literal(ScalarLiteral::Null),
            span: SpanId::default(),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum StmtCode {
    VarDecl(SymbolId, ExprId, bool),
    Assign(SymbolId, ExprId),
    If(ExprId, BlockId, BlockId),
    While(ExprId, BlockId),
    For(SymbolId, ExprId, BlockId),
    Break,
    Continue,
    Try(BlockId, BlockId),
    Return(Option<ExprId>),
    Expr(ExprId),
}

#[derive(Clone, Copy)]
pub(super) struct CodeStmt {
    pub(super) kind: StmtCode,
    pub(super) span: SpanId,
}

impl Default for CodeStmt {
    fn default() -> Self {
        Self {
            kind: StmtCode::Return(None),
            span: SpanId::default(),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Symbol {
    start: usize,
    len: usize,
}

#[derive(Debug)]
pub(super) enum AdmissionError {
    Unsupported(Span),
    Rejected(KuError),
}

impl AdmissionError {
    pub(super) fn into_error(self) -> KuError {
        match self {
            Self::Unsupported(span) => unsupported(span),
            Self::Rejected(error) => error,
        }
    }
}

type Admission<T> = Result<T, AdmissionError>;

// Selection is deliberately separate from arena allocation. A legal legacy
// construct near the end of a large body must not become a storage error just
// because a partially projected arena filled first. This streaming validation
// uses borrowed metadata only; an exhausted validation budget means selection
// is unknown, and production retains its existing evaluator for the whole tree.
pub(super) fn eligible_named(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> bool {
    Eligibility::new(source, None, limits)
        .check(FunctionSpec::named(entry))
        .is_ok()
}

pub(super) fn eligible_local(
    source: &HashMap<String, FnDecl>,
    call: &FunctionValueCall<'_>,
    limits: Limits,
) -> bool {
    let Some(name) = call.self_name.as_deref() else {
        return false;
    };
    Eligibility::new(source, Some((name, call.captures)), limits)
        .check(FunctionSpec {
            name,
            params: Parameters::Local(call.params),
            body: call.body,
            span: call.span,
            declaration: None,
        })
        .is_ok()
}

pub(super) struct CodeArena {
    functions: FrameStack<CodeFn>,
    statements: FrameStack<CodeStmt>,
    expressions: FrameStack<CodeExpr>,
    blocks: FrameStack<CodeBlock>,
    params: FrameStack<CodeParam>,
    spans: FrameStack<Span>,
    symbols: FrameStack<Symbol>,
    // UTF-8 bytes share the byte budget; a symbol, not each byte, is a logical
    // code frame. No String capacity or hidden hash table is left uncharged.
    names: FrameStack<u8>,
    construction_peak_backing_bytes_upper_bound: usize,
}

#[derive(Clone)]
pub(super) struct CodeHandle(Arc<CodeArena>);

impl std::fmt::Debug for CodeHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodeHandle")
            .field("functions", &self.0.functions.len())
            .field("retained_bytes", &self.retained_bytes())
            .finish()
    }
}

impl CodeHandle {
    pub(super) fn compile_named(
        source: &HashMap<String, FnDecl>,
        entry: &FnDecl,
        limits: Limits,
    ) -> Admission<Self> {
        Builder::new(source, None, limits).compile(Some(FunctionSpec::named(entry)))
    }

    #[cfg(test)]
    pub(super) fn compile_all(source: &HashMap<String, FnDecl>, limits: Limits) -> Admission<Self> {
        Builder::new(source, None, limits).compile(None)
    }

    pub(super) fn compile_local(
        source: &HashMap<String, FnDecl>,
        call: &FunctionValueCall<'_>,
        limits: Limits,
    ) -> Admission<Self> {
        let name = call
            .self_name
            .as_deref()
            .ok_or(AdmissionError::Unsupported(call.span))?;
        if call.is_async
            || source.contains_key(name)
            || call.params.len() != call.param_modes.len()
            || call
                .param_modes
                .iter()
                .any(|mode| *mode != ParamMode::Owned)
        {
            return Err(AdmissionError::Unsupported(call.span));
        }
        let spec = FunctionSpec {
            name,
            params: Parameters::Local(call.params),
            body: call.body,
            span: call.span,
            declaration: None,
        };
        Builder::new(source, Some((name, call.captures)), limits).compile(Some(spec))
    }

    pub(super) fn function(&self, id: FnId) -> CodeFn {
        *self.0.functions.get(id.0).expect("admitted function ID")
    }

    pub(super) fn find_function(&self, name: &str) -> Option<FnId> {
        (0..self.0.functions.len()).find_map(|index| {
            (self.symbol(self.function(FnId(index)).name) == name).then_some(FnId(index))
        })
    }

    pub(super) fn parameter(&self, function: CodeFn, index: usize) -> CodeParam {
        assert!(index < function.params.len);
        *self
            .0
            .params
            .get(function.params.start + index)
            .expect("parameter ID")
    }

    pub(super) fn statement(&self, id: StmtId) -> CodeStmt {
        *self.0.statements.get(id.0).expect("admitted statement ID")
    }

    pub(super) fn expression(&self, id: ExprId) -> CodeExpr {
        *self
            .0
            .expressions
            .get(id.0)
            .expect("admitted expression ID")
    }

    pub(super) fn block_statement(&self, id: BlockId, next: usize) -> Option<StmtId> {
        let block = self.0.blocks.get(id.0).expect("admitted block ID");
        (next < block.len).then(|| StmtId(block.start + next))
    }

    pub(super) fn block_span(&self, id: BlockId) -> SpanId {
        self.0.blocks.get(id.0).expect("admitted block ID").span
    }

    pub(super) fn span(&self, id: SpanId) -> Span {
        *self.0.spans.get(id.0).expect("admitted span ID")
    }

    pub(super) fn symbol(&self, id: SymbolId) -> &str {
        self.0.symbol(id)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.0.bytes() + owner_bytes()
    }

    pub(super) fn construction_peak_bytes(&self) -> usize {
        self.0.construction_peak_backing_bytes_upper_bound
    }
}

impl CodeArena {
    fn new(limits: FrameLimits) -> Self {
        Self {
            functions: FrameStack::new(limits),
            statements: FrameStack::new(limits),
            expressions: FrameStack::new(limits),
            blocks: FrameStack::new(limits),
            params: FrameStack::new(limits),
            spans: FrameStack::new(limits),
            symbols: FrameStack::new(limits),
            names: FrameStack::new(FrameLimits {
                max_frames: limits.max_storage_bytes,
                max_storage_bytes: limits.max_storage_bytes,
            }),
            construction_peak_backing_bytes_upper_bound: 0,
        }
    }

    fn count(&self) -> usize {
        self.functions.len()
            + self.statements.len()
            + self.expressions.len()
            + self.blocks.len()
            + self.params.len()
            + self.spans.len()
            + self.symbols.len()
    }

    fn bytes(&self) -> usize {
        self.functions.charged_storage_bytes()
            + self.statements.charged_storage_bytes()
            + self.expressions.charged_storage_bytes()
            + self.blocks.charged_storage_bytes()
            + self.params.charged_storage_bytes()
            + self.spans.charged_storage_bytes()
            + self.symbols.charged_storage_bytes()
            + self.names.charged_storage_bytes()
    }

    fn symbol(&self, id: SymbolId) -> &str {
        let symbol = self.symbols.get(id.0).expect("symbol ID");
        // Built from valid UTF-8 source, with no subsequent byte mutation.
        std::str::from_utf8(
            self.names
                .as_slice()
                .get(symbol.start..symbol.start + symbol.len)
                .expect("symbol byte range"),
        )
        .expect("source symbol UTF-8")
    }
}

fn owner_bytes() -> usize {
    // Arc's owned arena and two reference counters are part of this private
    // backing budget, excluding allocator headers as in FrameStack.
    std::mem::size_of::<CodeArena>() + 2 * std::mem::size_of::<usize>()
}

#[derive(Clone, Copy)]
enum Parameters<'a> {
    Named(&'a [Param]),
    Local(&'a [String]),
}

#[derive(Clone, Copy)]
struct FunctionSpec<'a> {
    name: &'a str,
    params: Parameters<'a>,
    body: &'a [Stmt],
    span: Span,
    declaration: Option<&'a FnDecl>,
}

impl<'a> FunctionSpec<'a> {
    fn named(function: &'a FnDecl) -> Self {
        Self {
            name: &function.name,
            params: Parameters::Named(&function.params),
            body: &function.body,
            span: function.span,
            declaration: Some(function),
        }
    }
}

enum Visit<'a> {
    Block {
        body: &'a [Stmt],
        id: BlockId,
        next: usize,
        bound_base: usize,
    },
    Expr(&'a Expr, ExprId),
    Bind(&'a str, bool, Span),
}

struct Builder<'a> {
    source: &'a HashMap<String, FnDecl>,
    local: Option<(&'a str, &'a Env)>,
    arena: CodeArena,
    functions: FrameStack<(FunctionSpec<'a>, FnId)>,
    pending: FrameStack<Visit<'a>>,
    bound: FrameStack<&'a str>,
    limits: FrameLimits,
    remaining: usize,
}

// Every candidate checks all retained buffers and its own old + new backing.
// Partially filled slots own only flat Copy metadata and UTF-8 bytes; dropping
// the failed builder has no source effects or recursive AST destruction.
macro_rules! reserve_field {
    ($builder:expr, $field:ident, $additional:expr, $span:expr) => {{
        let additional = $additional;
        let count = $builder
            .count()
            .checked_sub($builder.$field.len())
            .ok_or_else(|| rejected(ReserveError::ArithmeticOverflow, $span))?;
        let frames = $builder
            .limits
            .max_frames
            .checked_sub(count)
            .ok_or_else(|| rejected(ReserveError::FrameLimit, $span))?;
        let bytes = $builder
            .bytes()
            .checked_sub($builder.$field.charged_storage_bytes())
            .and_then(|other| $builder.limits.max_storage_bytes.checked_sub(other))
            .ok_or_else(|| rejected(ReserveError::StorageLimit, $span))?;
        $builder
            .$field
            .reserve_with_budget(additional, frames, bytes)
            .map_err(|error| rejected(error, $span))?;
    }};
    ($builder:expr, arena.$field:ident, $additional:expr, $span:expr) => {{
        let additional = $additional;
        let count = $builder
            .count()
            .checked_sub($builder.arena.$field.len())
            .ok_or_else(|| rejected(ReserveError::ArithmeticOverflow, $span))?;
        let frames = $builder
            .limits
            .max_frames
            .checked_sub(count)
            .ok_or_else(|| rejected(ReserveError::FrameLimit, $span))?;
        let bytes = $builder
            .bytes()
            .checked_sub($builder.arena.$field.charged_storage_bytes())
            .and_then(|other| $builder.limits.max_storage_bytes.checked_sub(other))
            .ok_or_else(|| rejected(ReserveError::StorageLimit, $span))?;
        $builder
            .arena
            .$field
            .reserve_with_budget(additional, frames, bytes)
            .map_err(|error| rejected(error, $span))?;
    }};
}

fn rejected(error: ReserveError, span: Span) -> AdmissionError {
    AdmissionError::Rejected(storage_error(error, span))
}

impl<'a> Builder<'a> {
    fn new(
        source: &'a HashMap<String, FnDecl>,
        local: Option<(&'a str, &'a Env)>,
        limits: Limits,
    ) -> Self {
        Self {
            source,
            local,
            arena: CodeArena::new(limits.syntax),
            functions: FrameStack::new(limits.syntax),
            pending: FrameStack::new(limits.syntax),
            bound: FrameStack::new(limits.syntax),
            limits: limits.syntax,
            remaining: limits.syntax_steps,
        }
    }

    fn count(&self) -> usize {
        self.arena.count() + self.functions.len() + self.pending.len() + self.bound.len()
    }

    fn bytes(&self) -> usize {
        self.arena.bytes()
            + self.functions.charged_storage_bytes()
            + self.pending.charged_storage_bytes()
            + self.bound.charged_storage_bytes()
            + owner_bytes()
    }

    fn spend(&mut self, count: usize, span: Span) -> Admission<()> {
        self.remaining = self
            .remaining
            .checked_sub(count)
            .ok_or_else(|| rejected(ReserveError::FrameLimit, span))?;
        Ok(())
    }

    fn span(&mut self, span: Span) -> Admission<SpanId> {
        reserve_field!(self, arena.spans, 1, span);
        let id = SpanId(self.arena.spans.len());
        push(&mut self.arena.spans, span);
        Ok(id)
    }

    fn symbol(&mut self, name: &str, span: Span) -> Admission<SymbolId> {
        for index in 0..self.arena.symbols.len() {
            self.spend(name.len().saturating_add(1), span)?;
            if self.arena.symbol(SymbolId(index)) == name {
                return Ok(SymbolId(index));
            }
        }
        self.spend(name.len(), span)?;
        reserve_field!(self, arena.symbols, 1, span);
        let bytes = self
            .bytes()
            .checked_sub(self.arena.names.charged_storage_bytes())
            .and_then(|other| self.limits.max_storage_bytes.checked_sub(other))
            .ok_or_else(|| rejected(ReserveError::StorageLimit, span))?;
        self.arena
            .names
            .reserve_with_budget(name.len(), self.limits.max_storage_bytes, bytes)
            .map_err(|error| rejected(error, span))?;
        let id = SymbolId(self.arena.symbols.len());
        let start = self.arena.names.len();
        for byte in name.bytes() {
            push(&mut self.arena.names, byte);
        }
        push(
            &mut self.arena.symbols,
            Symbol {
                start,
                len: name.len(),
            },
        );
        Ok(id)
    }

    fn ensure_function(&mut self, spec: FunctionSpec<'a>) -> Admission<FnId> {
        for index in 0..self.arena.functions.len() {
            self.spend(spec.name.len().saturating_add(1), spec.span)?;
            let function = self.arena.functions.get(index).expect("function slot");
            if self.arena.symbol(function.name) == spec.name {
                return Ok(FnId(index));
            }
        }
        if let Some(function) = spec.declaration {
            if function.is_async
                || !function.type_params.is_empty()
                || !scalar_type(&function.return_type)
            {
                return Err(AdmissionError::Unsupported(spec.span));
            }
        }
        let name = self.symbol(spec.name, spec.span)?;
        let span = self.span(spec.span)?;
        reserve_field!(self, arena.functions, 1, spec.span);
        let id = FnId(self.arena.functions.len());
        push(
            &mut self.arena.functions,
            CodeFn {
                name,
                span,
                ..CodeFn::default()
            },
        );
        reserve_field!(self, functions, 1, spec.span);
        push(&mut self.functions, (spec, id));
        Ok(id)
    }

    fn expressions(&mut self, count: usize, span: Span) -> Admission<ExprRange> {
        reserve_field!(self, arena.expressions, count, span);
        let start = self.arena.expressions.len();
        for _ in 0..count {
            push(&mut self.arena.expressions, CodeExpr::default());
        }
        Ok(ExprRange { start, len: count })
    }

    fn expression(&mut self, expr: &'a Expr) -> Admission<ExprId> {
        let id = self
            .expressions(1, expr.span)?
            .get(0)
            .expect("one expression");
        reserve_field!(self, pending, 1, expr.span);
        push(&mut self.pending, Visit::Expr(expr, id));
        Ok(id)
    }

    fn block(&mut self, body: &'a [Stmt], bound_base: usize) -> Admission<BlockId> {
        let span = body.first().map(stmt_span).unwrap_or_default();
        let span_id = self.span(span)?;
        reserve_field!(self, arena.statements, body.len(), span);
        let start = self.arena.statements.len();
        for _ in body {
            push(&mut self.arena.statements, CodeStmt::default());
        }
        reserve_field!(self, arena.blocks, 1, span);
        let id = BlockId(self.arena.blocks.len());
        push(
            &mut self.arena.blocks,
            CodeBlock {
                start,
                len: body.len(),
                span: span_id,
            },
        );
        reserve_field!(self, pending, 1, span);
        push(
            &mut self.pending,
            Visit::Block {
                body,
                id,
                next: 0,
                bound_base,
            },
        );
        Ok(id)
    }

    fn bind(&mut self, name: &'a str, span: Span) -> Admission<()> {
        reserve_field!(self, bound, 1, span);
        push(&mut self.bound, name);
        Ok(())
    }

    fn is_bound(&self, name: &str) -> bool {
        self.bound.as_slice().contains(&name)
    }

    fn check_free(&self, name: &str, span: Span) -> Admission<()> {
        if let Some((_, captures)) = self.local {
            if !self.is_bound(name)
                && (self.source.contains_key(name)
                    || (captures.contains(name) && !captures.is_stable_scalar_binding(name)))
            {
                return Err(AdmissionError::Unsupported(span));
            }
        }
        Ok(())
    }

    fn compile(mut self, entry: Option<FunctionSpec<'a>>) -> Admission<CodeHandle> {
        if self.bytes() > self.limits.max_storage_bytes {
            return Err(rejected(ReserveError::StorageLimit, Span::default()));
        }
        self.span(Span::default())?;
        if let Some(entry) = entry {
            self.ensure_function(entry)?;
        } else {
            for function in self.source.values() {
                self.ensure_function(FunctionSpec::named(function))?;
            }
        }
        while let Some((spec, id)) = self.functions.pop() {
            self.spend(1, spec.span)?;
            assert_eq!(self.pending.len(), 0);
            assert_eq!(self.bound.len(), 0);
            self.bind(spec.name, spec.span)?;
            let start = self.arena.params.len();
            let count = match spec.params {
                Parameters::Named(params) => params.len(),
                Parameters::Local(params) => params.len(),
            };
            reserve_field!(self, arena.params, count, spec.span);
            for _ in 0..count {
                push(&mut self.arena.params, CodeParam::default());
            }
            for index in 0..count {
                self.spend(1, spec.span)?;
                let (name, span) = match spec.params {
                    Parameters::Named(params) => {
                        let param = &params[index];
                        if param.mode != ParamMode::Owned || !scalar_type(&param.ty) {
                            return Err(AdmissionError::Unsupported(param.span));
                        }
                        (param.name.as_str(), param.span)
                    }
                    Parameters::Local(params) => (params[index].as_str(), spec.span),
                };
                if self.local.is_some() && (name == spec.name || self.is_bound(name)) {
                    return Err(AdmissionError::Unsupported(span));
                }
                let symbol = self.symbol(name, span)?;
                let span_id = self.span(span)?;
                *self
                    .arena
                    .params
                    .get_mut(start + index)
                    .expect("parameter slot") = CodeParam {
                    name: symbol,
                    span: span_id,
                };
                self.bind(name, span)?;
            }
            let body = self.block(spec.body, 0)?;
            let function = self.arena.functions.get_mut(id.0).expect("function slot");
            function.params = ParamRange { start, len: count };
            function.body = body;
            while let Some(visit) = self.pending.pop() {
                self.spend(1, spec.span)?;
                self.visit(visit)?;
            }
        }
        self.arena.construction_peak_backing_bytes_upper_bound = owner_bytes()
            + self.arena.functions.peak_storage_bytes()
            + self.arena.statements.peak_storage_bytes()
            + self.arena.expressions.peak_storage_bytes()
            + self.arena.blocks.peak_storage_bytes()
            + self.arena.params.peak_storage_bytes()
            + self.arena.spans.peak_storage_bytes()
            + self.arena.symbols.peak_storage_bytes()
            + self.arena.names.peak_storage_bytes()
            + self.functions.peak_storage_bytes()
            + self.pending.peak_storage_bytes()
            + self.bound.peak_storage_bytes();
        // The transient visit/binding/function buffers drop before publication;
        // all their capacities were charged while construction was in progress.
        // Arc::new follows standard allocator OOM behavior. Backing admission
        // does not promise recovery from global allocation failure.
        Ok(CodeHandle(Arc::new(self.arena)))
    }

    fn visit(&mut self, visit: Visit<'a>) -> Admission<()> {
        match visit {
            Visit::Block {
                body,
                id,
                next,
                bound_base,
            } => {
                let Some(stmt) = body.get(next) else {
                    while self.bound.len() > bound_base {
                        self.bound.pop();
                    }
                    return Ok(());
                };
                let span = stmt_span(stmt);
                reserve_field!(self, pending, 1, span);
                push(
                    &mut self.pending,
                    Visit::Block {
                        body,
                        id,
                        next: next + 1,
                        bound_base,
                    },
                );
                let span_id = self.span(span)?;
                let kind = self.statement(stmt)?;
                let block = self.arena.blocks.get(id.0).expect("block slot");
                *self
                    .arena
                    .statements
                    .get_mut(block.start + next)
                    .expect("statement slot") = CodeStmt {
                    kind,
                    span: span_id,
                };
            }
            Visit::Bind(name, assignment, span) => {
                if assignment {
                    self.check_free(name, span)?;
                }
                self.bind(name, span)?;
            }
            Visit::Expr(expr, id) => {
                let span = self.span(expr.span)?;
                let kind = match &expr.kind {
                    ExprKind::Literal(Literal::Int(value)) => {
                        ExprCode::Literal(ScalarLiteral::Int(*value))
                    }
                    ExprKind::Literal(Literal::Float(value)) => {
                        ExprCode::Literal(ScalarLiteral::Float(*value))
                    }
                    ExprKind::Literal(Literal::Bool(value)) => {
                        ExprCode::Literal(ScalarLiteral::Bool(*value))
                    }
                    ExprKind::Literal(Literal::Null) => ExprCode::Literal(ScalarLiteral::Null),
                    ExprKind::Variable(name) => {
                        if (self.local.is_none() && self.source.contains_key(name))
                            || self.local.is_some_and(|(self_name, _)| name == self_name)
                        {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        self.check_free(name, expr.span)?;
                        ExprCode::Variable(self.symbol(name, expr.span)?)
                    }
                    ExprKind::Unary { op, expr } => ExprCode::Unary(*op, self.expression(expr)?),
                    ExprKind::Binary { left, op, right } => {
                        let right = self.expression(right)?;
                        let left = self.expression(left)?;
                        ExprCode::Binary(left, *op, right)
                    }
                    ExprKind::Call { callee, args } => {
                        let ExprKind::Variable(name) = &callee.kind else {
                            return Err(AdmissionError::Unsupported(callee.span));
                        };
                        let function = if let Some((self_name, _)) = self.local {
                            if name != self_name {
                                return Err(AdmissionError::Unsupported(callee.span));
                            }
                            let function = *self.arena.functions.get(0).expect("local function");
                            if args.len() != function.arity() {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            FnId(0)
                        } else {
                            let function = self
                                .source
                                .get(name)
                                .ok_or(AdmissionError::Unsupported(callee.span))?;
                            self.ensure_function(FunctionSpec::named(function))?
                        };
                        let range = self.expressions(args.len(), expr.span)?;
                        reserve_field!(self, pending, args.len(), expr.span);
                        for (index, arg) in args.iter().enumerate().rev() {
                            push(
                                &mut self.pending,
                                Visit::Expr(arg, range.get(index).expect("argument ID")),
                            );
                        }
                        ExprCode::Call {
                            function,
                            args: range,
                            callee_span: self.span(callee.span)?,
                        }
                    }
                    _ => return Err(AdmissionError::Unsupported(expr.span)),
                };
                *self
                    .arena
                    .expressions
                    .get_mut(id.0)
                    .expect("expression slot") = CodeExpr { kind, span };
            }
        }
        Ok(())
    }

    fn statement(&mut self, stmt: &'a Stmt) -> Admission<StmtCode> {
        let span = stmt_span(stmt);
        if self.local.is_some_and(|(self_name, _)| matches!(stmt,
            Stmt::VarDecl { name, .. } | Stmt::Assign { name, .. } | Stmt::For { name, .. } if name == self_name)) {
            return Err(AdmissionError::Unsupported(span));
        }
        Ok(match stmt {
            Stmt::VarDecl {
                name,
                value,
                mutable,
                ty,
                ..
            } => {
                if !scalar_type(ty) {
                    return Err(AdmissionError::Unsupported(span));
                }
                reserve_field!(self, pending, 1, span);
                push(&mut self.pending, Visit::Bind(name, false, span));
                StmtCode::VarDecl(self.symbol(name, span)?, self.expression(value)?, *mutable)
            }
            Stmt::Assign { name, value, .. } => {
                reserve_field!(self, pending, 1, span);
                push(&mut self.pending, Visit::Bind(name, true, span));
                StmtCode::Assign(self.symbol(name, span)?, self.expression(value)?)
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let base = self.bound.len();
                let else_body = self.block(else_branch, base)?;
                let then_body = self.block(then_branch, base)?;
                StmtCode::If(self.expression(condition)?, then_body, else_body)
            }
            Stmt::While {
                condition, body, ..
            } => {
                let body = self.block(body, self.bound.len())?;
                StmtCode::While(self.expression(condition)?, body)
            }
            Stmt::For {
                name,
                iterable,
                body,
                ..
            } => {
                // The iterator exists only in the body, never the iterable.
                // Bind is deferred until that iterable has been inspected.
                let base = self.bound.len();
                let body = self.block(body, base)?;
                reserve_field!(self, pending, 1, span);
                push(&mut self.pending, Visit::Bind(name, false, span));
                StmtCode::For(self.symbol(name, span)?, self.expression(iterable)?, body)
            }
            Stmt::Break { .. } => StmtCode::Break,
            Stmt::Continue { .. } => StmtCode::Continue,
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                ..
            } => {
                if catch_name.is_some() || !catch_body.is_empty() {
                    return Err(AdmissionError::Unsupported(span));
                }
                let base = self.bound.len();
                let finally = self.block(finally_body, base)?;
                StmtCode::Try(self.block(body, base)?, finally)
            }
            Stmt::Return { value, .. } => StmtCode::Return(
                value
                    .as_ref()
                    .map(|value| self.expression(value))
                    .transpose()?,
            ),
            Stmt::Expr { expr, .. } => StmtCode::Expr(self.expression(expr)?),
            _ => return Err(AdmissionError::Unsupported(span)),
        })
    }
}

enum Scan<'a> {
    Block {
        body: &'a [Stmt],
        next: usize,
        base: usize,
    },
    Expr(&'a Expr),
    Bind(&'a str, bool, Span),
}

struct Eligibility<'a> {
    source: &'a HashMap<String, FnDecl>,
    local: Option<(&'a str, &'a Env)>,
    functions: FrameStack<FunctionSpec<'a>>,
    checked: FrameStack<&'a str>,
    pending: FrameStack<Scan<'a>>,
    bound: FrameStack<&'a str>,
    limits: FrameLimits,
    remaining: usize,
}

impl<'a> Eligibility<'a> {
    fn new(
        source: &'a HashMap<String, FnDecl>,
        local: Option<(&'a str, &'a Env)>,
        limits: Limits,
    ) -> Self {
        Self {
            source,
            local,
            functions: FrameStack::new(limits.syntax),
            checked: FrameStack::new(limits.syntax),
            pending: FrameStack::new(limits.syntax),
            bound: FrameStack::new(limits.syntax),
            limits: limits.syntax,
            remaining: limits.syntax_steps,
        }
    }

    fn count(&self) -> usize {
        self.functions.len() + self.checked.len() + self.pending.len() + self.bound.len()
    }
    fn bytes(&self) -> usize {
        self.functions.charged_storage_bytes()
            + self.checked.charged_storage_bytes()
            + self.pending.charged_storage_bytes()
            + self.bound.charged_storage_bytes()
    }
    fn spend(&mut self, count: usize, span: Span) -> Admission<()> {
        self.remaining = self
            .remaining
            .checked_sub(count)
            .ok_or_else(|| rejected(ReserveError::FrameLimit, span))?;
        Ok(())
    }
    fn bound(&self, name: &str) -> bool {
        self.bound.as_slice().contains(&name)
    }
    fn bind(&mut self, name: &'a str, span: Span) -> Admission<()> {
        reserve_field!(self, bound, 1, span);
        push(&mut self.bound, name);
        Ok(())
    }
    fn free(&self, name: &str, span: Span) -> Admission<()> {
        if let Some((_, captures)) = self.local {
            if !self.bound(name)
                && (self.source.contains_key(name)
                    || (captures.contains(name) && !captures.is_stable_scalar_binding(name)))
            {
                return Err(AdmissionError::Unsupported(span));
            }
        }
        Ok(())
    }
    fn function(&mut self, spec: FunctionSpec<'a>) -> Admission<()> {
        if self.checked.as_slice().contains(&spec.name) {
            return Ok(());
        }
        if let Some(function) = spec.declaration {
            if function.is_async
                || !function.type_params.is_empty()
                || !scalar_type(&function.return_type)
                || function
                    .params
                    .iter()
                    .any(|param| param.mode != ParamMode::Owned || !scalar_type(&param.ty))
            {
                return Err(AdmissionError::Unsupported(spec.span));
            }
        }
        reserve_field!(self, checked, 1, spec.span);
        push(&mut self.checked, spec.name);
        reserve_field!(self, functions, 1, spec.span);
        push(&mut self.functions, spec);
        Ok(())
    }
    fn expression(&mut self, expr: &'a Expr) -> Admission<()> {
        reserve_field!(self, pending, 1, expr.span);
        push(&mut self.pending, Scan::Expr(expr));
        Ok(())
    }
    fn block(&mut self, body: &'a [Stmt], base: usize) -> Admission<()> {
        let span = body.first().map(stmt_span).unwrap_or_default();
        reserve_field!(self, pending, 1, span);
        push(
            &mut self.pending,
            Scan::Block {
                body,
                next: 0,
                base,
            },
        );
        Ok(())
    }
    fn check(mut self, entry: FunctionSpec<'a>) -> Admission<()> {
        self.function(entry)?;
        while let Some(spec) = self.functions.pop() {
            self.spend(1, spec.span)?;
            assert_eq!(self.bound.len(), 0);
            self.bind(spec.name, spec.span)?;
            let count = match spec.params {
                Parameters::Named(params) => params.len(),
                Parameters::Local(params) => params.len(),
            };
            self.spend(count, spec.span)?;
            for index in 0..count {
                let name = match spec.params {
                    Parameters::Named(params) => params[index].name.as_str(),
                    Parameters::Local(params) => params[index].as_str(),
                };
                if self.local.is_some() && (name == spec.name || self.bound(name)) {
                    return Err(AdmissionError::Unsupported(spec.span));
                }
                self.bind(name, spec.span)?;
            }
            self.block(spec.body, 0)?;
            while let Some(scan) = self.pending.pop() {
                self.spend(1, spec.span)?;
                match scan {
                    Scan::Block { body, next, base } => {
                        let Some(stmt) = body.get(next) else {
                            while self.bound.len() > base {
                                self.bound.pop();
                            }
                            continue;
                        };
                        let span = stmt_span(stmt);
                        reserve_field!(self, pending, 1, span);
                        push(
                            &mut self.pending,
                            Scan::Block {
                                body,
                                next: next + 1,
                                base,
                            },
                        );
                        if self.local.is_some_and(|(self_name, _)| matches!(stmt,
                            Stmt::VarDecl { name, .. } | Stmt::Assign { name, .. } | Stmt::For { name, .. } if name == self_name)) {
                            return Err(AdmissionError::Unsupported(span));
                        }
                        match stmt {
                            Stmt::VarDecl {
                                name, ty, value, ..
                            } => {
                                if !scalar_type(ty) {
                                    return Err(AdmissionError::Unsupported(span));
                                }
                                reserve_field!(self, pending, 1, span);
                                push(&mut self.pending, Scan::Bind(name, false, span));
                                self.expression(value)?;
                            }
                            Stmt::Assign { name, value, .. } => {
                                reserve_field!(self, pending, 1, span);
                                push(&mut self.pending, Scan::Bind(name, true, span));
                                self.expression(value)?;
                            }
                            Stmt::Return { value, .. } => {
                                if let Some(value) = value {
                                    self.expression(value)?;
                                }
                            }
                            Stmt::Expr { expr, .. } => self.expression(expr)?,
                            Stmt::If {
                                condition,
                                then_branch,
                                else_branch,
                                ..
                            } => {
                                let base = self.bound.len();
                                self.block(else_branch, base)?;
                                self.block(then_branch, base)?;
                                self.expression(condition)?;
                            }
                            Stmt::While {
                                condition, body, ..
                            } => {
                                self.block(body, self.bound.len())?;
                                self.expression(condition)?;
                            }
                            Stmt::For {
                                name,
                                iterable,
                                body,
                                ..
                            } => {
                                self.block(body, self.bound.len())?;
                                reserve_field!(self, pending, 1, span);
                                push(&mut self.pending, Scan::Bind(name, false, span));
                                self.expression(iterable)?;
                            }
                            Stmt::Break { .. } | Stmt::Continue { .. } => {}
                            Stmt::Try {
                                body,
                                catch_name,
                                catch_body,
                                finally_body,
                                ..
                            } => {
                                if catch_name.is_some() || !catch_body.is_empty() {
                                    return Err(AdmissionError::Unsupported(span));
                                }
                                let base = self.bound.len();
                                self.block(finally_body, base)?;
                                self.block(body, base)?;
                            }
                            _ => return Err(AdmissionError::Unsupported(span)),
                        }
                    }
                    Scan::Bind(name, assignment, span) => {
                        if assignment {
                            self.free(name, span)?;
                        }
                        self.bind(name, span)?;
                    }
                    Scan::Expr(expr) => match &expr.kind {
                        ExprKind::Literal(
                            Literal::Int(_) | Literal::Float(_) | Literal::Bool(_) | Literal::Null,
                        ) => {}
                        ExprKind::Variable(name) => {
                            if (self.local.is_none() && self.source.contains_key(name))
                                || self.local.is_some_and(|(self_name, _)| name == self_name)
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            self.free(name, expr.span)?;
                        }
                        ExprKind::Unary { expr, .. } => self.expression(expr)?,
                        ExprKind::Binary { left, right, .. } => {
                            self.expression(right)?;
                            self.expression(left)?;
                        }
                        ExprKind::Call { callee, args } => {
                            let ExprKind::Variable(name) = &callee.kind else {
                                return Err(AdmissionError::Unsupported(callee.span));
                            };
                            if let Some((self_name, _)) = self.local {
                                if name != self_name || args.len() != count {
                                    return Err(AdmissionError::Unsupported(expr.span));
                                }
                            } else {
                                let function = self
                                    .source
                                    .get(name)
                                    .ok_or(AdmissionError::Unsupported(callee.span))?;
                                self.function(FunctionSpec::named(function))?;
                            }
                            reserve_field!(self, pending, args.len(), expr.span);
                            for arg in args.iter().rev() {
                                push(&mut self.pending, Scan::Expr(arg));
                            }
                        }
                        _ => return Err(AdmissionError::Unsupported(expr.span)),
                    },
                }
            }
        }
        Ok(())
    }
}

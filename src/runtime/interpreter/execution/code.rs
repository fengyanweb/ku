//! Owned, flat syntax for selected scalar trees, static scalar async
//! dependencies and synchronous root-only String loans.
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
    pub(super) owned_string: bool,
    pub(super) view_string: bool,
}

#[derive(Clone, Copy, Default)]
pub(super) struct CodeFn {
    pub(super) name: SymbolId,
    params: ParamRange,
    pub(super) body: BlockId,
    pub(super) span: SpanId,
    pub(super) is_async: bool,
    pub(super) owned_async_string: bool,
    pub(super) owned_scalar_result: Option<PrimitiveResultKind>,
    result_contract: ResultContract,
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
    // Only admitted Task bindings use the evaluator's owning read. Scalar
    // reads retain their original Copy path; Result locals remain legacy.
    VariableOwning(SymbolId),
    // A checked consuming use of a named String binding. This move never
    // broadens scalar Copy reads or the existing Task owning-read contract.
    VariableStringOwning(SymbolId),
    // Non-consuming reads of a synchronous selected primitive Result local
    // retain the original Env clone contract, with pre-allocation admission.
    VariableSelectedScalarResultClone(SymbolId),
    // Non-consuming reads of the synchronous one-input String Result slice.
    // The runtime admits the real clone buffer and Box before publication.
    VariableSelectedStringResultClone(SymbolId),
    // Explicit zero-argument clone on the same selected local keeps both
    // the method-call tick and target-variable tick, with one real copy.
    SelectedStringResultCloneCall(ExprId),
    // Root-only synchronous String loans; the real lease is owned by the
    // surrounding argument frame, rather than this shared syntax owner.
    StringBorrow(SymbolId),
    StringLength {
        symbol: SymbolId,
        byte_len: bool,
        target_span: SpanId,
    },
    Unary(UnaryOp, ExprId),
    Binary(ExprId, BinaryOp, ExprId),
    // Only a direct Result return constructs this builtin. The String
    // operand is enabled solely by the strict owned Result<String> role.
    TerminalOk(ExprId, bool),
    // Synchronous Owned String inputs may return one charged primitive
    // Result. The declared kind constrains static relay/cache contracts;
    // actual scalar variants retain the existing Unknown-call behavior.
    TerminalOwnedScalarOk(ExprId, PrimitiveResultKind),
    // Only an unshadowed direct return in a charged owned String or primitive
    // Result role. The builtin keeps its original String clone conversion.
    TerminalErr(ExprId),
    // Synchronous borrowed primitive Result returns use their own charged
    // wrapper; old scalar Result roots retain their original contract.
    TerminalBorrowedOk(ExprId),
    Call {
        function: FnId,
        args: ExprRange,
        callee_span: SpanId,
    },
    AsyncStart {
        function: FnId,
        args: ExprRange,
    },
    AsyncSelfStart {
        function: FnId,
        args: ExprRange,
        callee_span: SpanId,
    },
    Await(ExprId),
    // Shared terminal await opcode for charged owned String and primitive
    // Results. The selected ticket admits its real payload before transfer.
    TerminalAwaitOwnedString(ExprId),
    Propagate(ExprId),
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
// uses borrowed metadata only. Existing generic selectors treat exhausted
// validation as unknown selection. Strict owned async Result admission keeps
// Unsupported separate from a controlled budget Rejected outcome.
pub(super) fn eligible_named(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> bool {
    Eligibility::new(source, None, limits)
        .check(FunctionSpec::named_root(entry))
        .is_ok()
}

pub(super) fn eligible_owned_named(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> bool {
    Eligibility::new(source, None, limits)
        .check(FunctionSpec::owned_named(entry))
        .is_ok()
}

pub(super) fn eligible_borrowed_string_named(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> bool {
    Eligibility::new(source, None, limits)
        .check(FunctionSpec::borrowed_string_named(entry))
        .is_ok()
}

pub(super) fn eligible_claimed_async_entry(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> bool {
    Eligibility::new(source, None, limits)
        .check(FunctionSpec::claimed_async_entry(entry))
        .is_ok()
}

pub(super) fn admit_claimed_async_owned_result_string_entry(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> Admission<()> {
    Eligibility::new(source, None, limits)
        .check(FunctionSpec::claimed_async_owned_result_string_entry(entry))
}

pub(super) fn eligible_local(
    source: &HashMap<String, FnDecl>,
    call: &FunctionValueCall<'_>,
    limits: Limits,
) -> bool {
    let Some(name) = call.self_name.as_deref() else {
        return false;
    };
    Eligibility::new(
        source,
        Some(LocalContext {
            name,
            captures: call.captures,
            is_async: false,
        }),
        limits,
    )
    .check(FunctionSpec {
        name,
        params: Parameters::Local(call.params),
        body: call.body,
        span: call.span,
        declaration: None,
        role: FunctionRole::Scalar,
    })
    .is_ok()
}

pub(super) fn admit_claimed_async_owned_scalar_result_entry(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> Admission<()> {
    Eligibility::new(source, None, limits)
        .check(FunctionSpec::claimed_async_owned_scalar_result_entry(entry))
}

#[cfg(test)]
pub(super) fn eligible_claimed_async_owned_result_string_entry(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> bool {
    admit_claimed_async_owned_result_string_entry(source, entry, limits).is_ok()
}

#[cfg(test)]
pub(super) fn eligible_claimed_async_owned_scalar_result_entry(
    source: &HashMap<String, FnDecl>,
    entry: &FnDecl,
    limits: Limits,
) -> bool {
    admit_claimed_async_owned_scalar_result_entry(source, entry, limits).is_ok()
}

pub(super) fn eligible_claimed_async_local(
    source: &HashMap<String, FnDecl>,
    call: &FunctionValueCall<'_>,
    limits: Limits,
) -> bool {
    let Ok((local, spec)) = claimed_async_local_spec(source, call) else {
        return false;
    };
    Eligibility::new(source, Some(local), limits)
        .check(spec)
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
    // A private whole-graph fact, distinct from a function cache contract.
    // Terminal-only old source keeps its original runtime path and hooks.
    has_actual_string_result_locals: bool,
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
        Builder::new(source, None, limits).compile(Some(FunctionSpec::named_root(entry)))
    }

    pub(super) fn compile_owned_named(
        source: &HashMap<String, FnDecl>,
        entry: &FnDecl,
        limits: Limits,
    ) -> Admission<Self> {
        Builder::new(source, None, limits).compile(Some(FunctionSpec::owned_named(entry)))
    }

    pub(super) fn compile_borrowed_string_named(
        source: &HashMap<String, FnDecl>,
        entry: &FnDecl,
        limits: Limits,
    ) -> Admission<Self> {
        Builder::new(source, None, limits).compile(Some(FunctionSpec::borrowed_string_named(entry)))
    }

    pub(super) fn compile_claimed_async_entry(
        source: &HashMap<String, FnDecl>,
        entry: &FnDecl,
        limits: Limits,
    ) -> Admission<Self> {
        Builder::new(source, None, limits).compile(Some(FunctionSpec::claimed_async_entry(entry)))
    }

    pub(super) fn compile_claimed_async_owned_result_string_entry(
        source: &HashMap<String, FnDecl>,
        entry: &FnDecl,
        limits: Limits,
    ) -> Admission<Self> {
        Builder::new(source, None, limits).compile(Some(
            FunctionSpec::claimed_async_owned_result_string_entry(entry),
        ))
    }

    pub(super) fn compile_claimed_async_owned_scalar_result_entry(
        source: &HashMap<String, FnDecl>,
        entry: &FnDecl,
        limits: Limits,
    ) -> Admission<Self> {
        Builder::new(source, None, limits).compile(Some(
            FunctionSpec::claimed_async_owned_scalar_result_entry(entry),
        ))
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
            role: FunctionRole::Scalar,
        };
        Builder::new(
            source,
            Some(LocalContext {
                name,
                captures: call.captures,
                is_async: false,
            }),
            limits,
        )
        .compile(Some(spec))
    }

    pub(super) fn compile_claimed_async_local(
        source: &HashMap<String, FnDecl>,
        call: &FunctionValueCall<'_>,
        limits: Limits,
    ) -> Admission<Self> {
        let (local, spec) = claimed_async_local_spec(source, call)?;
        Builder::new(source, Some(local), limits).compile(Some(spec))
    }

    pub(super) fn function(&self, id: FnId) -> CodeFn {
        *self.0.functions.get(id.0).expect("admitted function ID")
    }

    pub(super) fn has_actual_string_result_locals(&self) -> bool {
        self.0.has_actual_string_result_locals
    }

    pub(super) fn uses_local_string_results(&self, id: FnId) -> bool {
        self.function(id).result_contract == ResultContract::OwnedStringLocals
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
            has_actual_string_result_locals: false,
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
enum FunctionRole {
    Scalar,
    OwnedNamed,
    OwnedResultStringNamed,
    OwnedLocalResultStringNamed,
    OwnedScalarResultNamed,
    BorrowedStringNamed,
    BorrowedScalarResultNamed,
    ResultRoot,
    ClaimedAsyncResultRoot,
    ClaimedAsyncOwnedResultStringRoot,
    AsyncOwnedResultStringDependency,
    ClaimedAsyncOwnedScalarResultRoot,
    AsyncOwnedScalarResultDependency,
    AsyncResultDependency,
    ClaimedLocalAsyncResultRoot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrimitiveResultKind {
    Int,
    Float,
    Bool,
    Null,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ResultContract {
    #[default]
    None,
    UnchargedScalar(Option<PrimitiveResultKind>),
    BorrowedScalar(PrimitiveResultKind),
    OwnedScalar,
    OwnedString,
    OwnedStringLocals,
}

type FunctionContract = (bool, ResultContract, Option<PrimitiveResultKind>);

#[derive(Clone, Copy)]
struct LocalContext<'a> {
    name: &'a str,
    captures: &'a Env,
    is_async: bool,
}

impl LocalContext<'_> {
    fn stable_capture(self, name: &str) -> bool {
        if self.is_async {
            // The producer's mutable cell becomes foreign to every child.
            // Its same-Task synchronous stability cannot prove async safety.
            self.captures.is_immutable_scalar_binding(name)
        } else {
            self.captures.is_stable_scalar_binding(name)
        }
    }
}

#[derive(Clone, Copy)]
struct FunctionSpec<'a> {
    name: &'a str,
    params: Parameters<'a>,
    body: &'a [Stmt],
    span: Span,
    declaration: Option<&'a FnDecl>,
    role: FunctionRole,
}

fn claimed_async_local_spec<'a>(
    source: &HashMap<String, FnDecl>,
    call: &'a FunctionValueCall<'_>,
) -> Admission<(LocalContext<'a>, FunctionSpec<'a>)> {
    let name = call
        .self_name
        .as_deref()
        .ok_or(AdmissionError::Unsupported(call.span))?;
    if !call.is_async
        || source.contains_key(name)
        || call.params.len() != call.args.len()
        || call.params.len() != call.param_modes.len()
        || call
            .param_modes
            .iter()
            .any(|mode| *mode != ParamMode::Owned)
        || call.args.iter().any(|value| {
            !matches!(
                value,
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null
            )
        })
    {
        return Err(AdmissionError::Unsupported(call.span));
    }
    // Borrow params/body/self metadata while the producer still owns its real
    // callable. The builder copies only flat syntax; it never clones that AST,
    // an argument, or a capture payload. Unsupported admission remains whole
    // legacy, including its original child-side arity and error timing.
    Ok((
        LocalContext {
            name,
            captures: call.captures,
            is_async: true,
        },
        FunctionSpec {
            name,
            params: Parameters::Local(call.params),
            body: call.body,
            span: call.span,
            declaration: None,
            role: FunctionRole::ClaimedLocalAsyncResultRoot,
        },
    ))
}

impl<'a> FunctionSpec<'a> {
    fn named(function: &'a FnDecl) -> Self {
        Self {
            name: &function.name,
            params: Parameters::Named(&function.params),
            body: &function.body,
            span: function.span,
            declaration: Some(function),
            role: FunctionRole::Scalar,
        }
    }

    fn named_root(function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        if scalar_result_type(&function.return_type) {
            spec.role = FunctionRole::ResultRoot;
        }
        spec
    }

    fn owned_named(function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        spec.role = if string_result_type(&function.return_type) {
            if has_single_owned_string_parameter(function) {
                FunctionRole::OwnedLocalResultStringNamed
            } else {
                FunctionRole::OwnedResultStringNamed
            }
        } else if scalar_result_type(&function.return_type) {
            FunctionRole::OwnedScalarResultNamed
        } else {
            FunctionRole::OwnedNamed
        };
        spec
    }

    fn owned_dependency(self, function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        spec.role = if self.owns_result_string() && string_result_type(&function.return_type) {
            // Keep the one-input Machine capability through terminal-only
            // dependencies too. Otherwise a multi-String relay back to this
            // root would conflict with its already cached charging contract.
            // Actual local storage still requires one owned String parameter.
            if matches!(self.role, FunctionRole::OwnedLocalResultStringNamed) {
                FunctionRole::OwnedLocalResultStringNamed
            } else {
                FunctionRole::OwnedResultStringNamed
            }
        } else if self.owns_scalar_result() && scalar_result_type(&function.return_type) {
            FunctionRole::OwnedScalarResultNamed
        } else {
            FunctionRole::OwnedNamed
        };
        spec
    }

    fn owns_result_string(self) -> bool {
        matches!(
            self.role,
            FunctionRole::OwnedResultStringNamed
                | FunctionRole::OwnedLocalResultStringNamed
                | FunctionRole::ClaimedAsyncOwnedResultStringRoot
                | FunctionRole::AsyncOwnedResultStringDependency
        )
    }

    fn owns_scalar_result(self) -> bool {
        matches!(
            self.role,
            FunctionRole::OwnedScalarResultNamed
                | FunctionRole::ClaimedAsyncOwnedScalarResultRoot
                | FunctionRole::AsyncOwnedScalarResultDependency
        )
    }

    fn owned_scalar_result_kind(self) -> Option<PrimitiveResultKind> {
        self.declaration
            .filter(|_| self.owns_scalar_result())
            .and_then(|function| primitive_result_kind(&function.return_type))
    }

    fn stores_string_result(self) -> bool {
        matches!(self.role, FunctionRole::OwnedLocalResultStringNamed)
            && self
                .declaration
                .is_some_and(has_single_owned_string_parameter)
    }

    fn check_string_local(self, span: Span) -> Admission<()> {
        if self.stores_string_result() {
            Ok(())
        } else {
            Err(AdmissionError::Unsupported(span))
        }
    }

    fn stores_scalar_result(self) -> bool {
        matches!(
            self.role,
            FunctionRole::OwnedScalarResultNamed
                | FunctionRole::ClaimedAsyncOwnedScalarResultRoot
                | FunctionRole::AsyncOwnedScalarResultDependency
        )
    }

    fn check_scalar_local_kind(self, kind: PrimitiveResultKind, span: Span) -> Admission<()> {
        if self.stores_scalar_result() && self.owned_scalar_result_kind() == Some(kind) {
            Ok(())
        } else {
            Err(AdmissionError::Unsupported(span))
        }
    }

    fn cache_contract(self) -> FunctionContract {
        let primitive = self
            .declaration
            .and_then(|function| primitive_result_kind(&function.return_type));
        let result = if matches!(self.role, FunctionRole::OwnedLocalResultStringNamed) {
            ResultContract::OwnedStringLocals
        } else if self.owns_result_string() {
            ResultContract::OwnedString
        } else if self.owns_scalar_result() {
            ResultContract::OwnedScalar
        } else if self.borrows_scalar_result() {
            ResultContract::BorrowedScalar(primitive.expect("validated borrowed Result"))
        } else if self.is_result_root() {
            ResultContract::UnchargedScalar(primitive)
        } else {
            ResultContract::None
        };
        (self.is_async(), result, self.owned_scalar_result_kind())
    }

    fn borrowed_string_named(function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        spec.role = if scalar_result_type(&function.return_type) {
            FunctionRole::BorrowedScalarResultNamed
        } else {
            FunctionRole::BorrowedStringNamed
        };
        spec
    }

    fn borrowed_dependency(self, function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        spec.role = if self.borrows_scalar_result() && scalar_result_type(&function.return_type) {
            FunctionRole::BorrowedScalarResultNamed
        } else {
            FunctionRole::BorrowedStringNamed
        };
        spec
    }

    fn borrows_scalar_result(self) -> bool {
        matches!(self.role, FunctionRole::BorrowedScalarResultNamed)
    }

    fn claimed_async_entry(function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        spec.role = FunctionRole::ClaimedAsyncResultRoot;
        spec
    }

    fn claimed_async_owned_result_string_entry(function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        spec.role = FunctionRole::ClaimedAsyncOwnedResultStringRoot;
        spec
    }

    fn claimed_async_owned_scalar_result_entry(function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        spec.role = FunctionRole::ClaimedAsyncOwnedScalarResultRoot;
        spec
    }

    fn is_claimed_async_owned_result_string(self) -> bool {
        matches!(
            self.role,
            FunctionRole::ClaimedAsyncOwnedResultStringRoot
                | FunctionRole::AsyncOwnedResultStringDependency
        )
    }

    fn is_claimed_async_owned_result(self) -> bool {
        self.is_claimed_async_owned_result_string()
            || matches!(
                self.role,
                FunctionRole::ClaimedAsyncOwnedScalarResultRoot
                    | FunctionRole::AsyncOwnedScalarResultDependency
            )
    }

    fn check_owned_async_target(self, function: &FnDecl) -> Admission<()> {
        let same_result = if let Some(kind) = self.owned_scalar_result_kind() {
            primitive_result_kind(&function.return_type) == Some(kind)
        } else {
            self.owns_result_string() && string_result_type(&function.return_type)
        };
        if function.is_async && same_result {
            Ok(())
        } else {
            Err(AdmissionError::Unsupported(function.span))
        }
    }

    fn check_terminal_owned_await(
        self,
        task: &Expr,
        source: &HashMap<String, FnDecl>,
    ) -> Admission<()> {
        let ExprKind::Call { callee, .. } = &task.kind else {
            return Err(AdmissionError::Unsupported(task.span));
        };
        let ExprKind::Variable(name) = &callee.kind else {
            return Err(AdmissionError::Unsupported(callee.span));
        };
        let function = source
            .get(name)
            .ok_or(AdmissionError::Unsupported(callee.span))?;
        self.check_owned_async_target(function)
    }

    fn owned_async_dependency(self, function: &'a FnDecl) -> Admission<Self> {
        self.check_owned_async_target(function)?;
        let mut spec = Self::named(function);
        spec.role = if self.owns_scalar_result() {
            FunctionRole::AsyncOwnedScalarResultDependency
        } else {
            FunctionRole::AsyncOwnedResultStringDependency
        };
        Ok(spec)
    }

    fn allows_await(self) -> bool {
        // Charged owned async Result roots/dependencies use only their separately
        // checked direct terminal await, never the general scalar Await.
        self.is_async() && !self.is_claimed_async_owned_result()
    }

    fn dependency(function: &'a FnDecl) -> Self {
        let mut spec = Self::named(function);
        if function.is_async {
            spec.role = FunctionRole::AsyncResultDependency;
        }
        spec
    }

    fn is_async(self) -> bool {
        matches!(
            self.role,
            FunctionRole::ClaimedAsyncResultRoot
                | FunctionRole::ClaimedAsyncOwnedResultStringRoot
                | FunctionRole::AsyncOwnedResultStringDependency
                | FunctionRole::ClaimedAsyncOwnedScalarResultRoot
                | FunctionRole::AsyncOwnedScalarResultDependency
                | FunctionRole::AsyncResultDependency
                | FunctionRole::ClaimedLocalAsyncResultRoot
        )
    }

    fn owns_strings(self) -> bool {
        matches!(
            self.role,
            FunctionRole::OwnedNamed
                | FunctionRole::OwnedResultStringNamed
                | FunctionRole::OwnedLocalResultStringNamed
                | FunctionRole::OwnedScalarResultNamed
                | FunctionRole::ClaimedAsyncOwnedResultStringRoot
                | FunctionRole::AsyncOwnedResultStringDependency
                | FunctionRole::ClaimedAsyncOwnedScalarResultRoot
                | FunctionRole::AsyncOwnedScalarResultDependency
                | FunctionRole::BorrowedStringNamed
                | FunctionRole::BorrowedScalarResultNamed
        )
    }

    fn borrows_strings(self) -> bool {
        matches!(
            self.role,
            FunctionRole::BorrowedStringNamed | FunctionRole::BorrowedScalarResultNamed
        )
    }

    fn parameter_kind(self, param: &Param) -> Admission<ValueKind> {
        if param.mode == ParamMode::Owned {
            if scalar_type(&param.ty) {
                return Ok(ValueKind::Scalar);
            }
            if self.owns_strings() && !self.borrows_scalar_result() && string_type(&param.ty) {
                return Ok(ValueKind::String);
            }
        } else if param.mode == ParamMode::View && self.borrows_strings() && string_type(&param.ty)
        {
            return Ok(ValueKind::ViewString);
        }
        Err(AdmissionError::Unsupported(param.span))
    }

    fn return_kind(self) -> ValueKind {
        if self.owns_result_string() {
            return ValueKind::ResultString;
        }
        if let Some(kind) = self.owned_scalar_result_kind() {
            return ValueKind::ScalarResult(kind);
        }
        if self.owns_strings()
            && self
                .declaration
                .is_some_and(|function| string_type(&function.return_type))
        {
            ValueKind::String
        } else {
            ValueKind::Scalar
        }
    }

    fn is_result_root(self) -> bool {
        matches!(
            self.role,
            FunctionRole::ResultRoot
                | FunctionRole::BorrowedScalarResultNamed
                | FunctionRole::ClaimedAsyncResultRoot
                | FunctionRole::AsyncResultDependency
                | FunctionRole::ClaimedLocalAsyncResultRoot
        )
    }

    fn validate(self) -> Admission<()> {
        let Some(function) = self.declaration else {
            return match self.role {
                FunctionRole::Scalar | FunctionRole::ClaimedLocalAsyncResultRoot => Ok(()),
                _ => Err(AdmissionError::Unsupported(self.span)),
            };
        };
        let return_supported = match self.role {
            FunctionRole::Scalar => !function.is_async && scalar_type(&function.return_type),
            FunctionRole::OwnedNamed | FunctionRole::BorrowedStringNamed => {
                !function.is_async
                    && (scalar_type(&function.return_type) || string_type(&function.return_type))
            }
            FunctionRole::BorrowedScalarResultNamed => {
                !function.is_async && scalar_result_type(&function.return_type)
            }
            FunctionRole::OwnedResultStringNamed | FunctionRole::OwnedLocalResultStringNamed => {
                !function.is_async && string_result_type(&function.return_type)
            }
            FunctionRole::OwnedScalarResultNamed => {
                !function.is_async && scalar_result_type(&function.return_type)
            }
            FunctionRole::ClaimedAsyncOwnedResultStringRoot
            | FunctionRole::AsyncOwnedResultStringDependency => {
                function.is_async && string_result_type(&function.return_type)
            }
            FunctionRole::ClaimedAsyncOwnedScalarResultRoot
            | FunctionRole::AsyncOwnedScalarResultDependency => {
                function.is_async && scalar_result_type(&function.return_type)
            }
            FunctionRole::ResultRoot => {
                !function.is_async && scalar_result_type(&function.return_type)
            }
            FunctionRole::ClaimedAsyncResultRoot | FunctionRole::AsyncResultDependency => {
                function.is_async && scalar_result_type(&function.return_type)
            }
            FunctionRole::ClaimedLocalAsyncResultRoot => false,
        };
        if !function.type_params.is_empty() || !return_supported {
            return Err(AdmissionError::Unsupported(self.span));
        }
        if self.borrows_strings() && mixed_string_parameter_modes(function) {
            return Err(AdmissionError::Unsupported(self.span));
        }
        for param in &function.params {
            self.parameter_kind(param)?;
        }
        Ok(())
    }
}

fn string_type(ty: &Option<TypeName>) -> bool {
    matches!(ty, Some(TypeName::String))
}

fn string_result_type(ty: &Option<TypeName>) -> bool {
    matches!(ty, Some(TypeName::Result(inner)) if matches!(inner.as_ref(), TypeName::String))
}

fn has_single_owned_string_parameter(function: &FnDecl) -> bool {
    function
        .params
        .iter()
        .filter(|param| string_type(&param.ty))
        .count()
        == 1
        && function
            .params
            .iter()
            .filter(|param| string_type(&param.ty))
            .all(|param| param.mode == ParamMode::Owned)
}

fn scalar_result_type(ty: &Option<TypeName>) -> bool {
    primitive_result_kind(ty).is_some()
}

fn primitive_result_kind(ty: &Option<TypeName>) -> Option<PrimitiveResultKind> {
    let Some(TypeName::Result(inner)) = ty else {
        return None;
    };
    Some(match inner.as_ref() {
        TypeName::Int => PrimitiveResultKind::Int,
        TypeName::Float => PrimitiveResultKind::Float,
        TypeName::Bool => PrimitiveResultKind::Bool,
        TypeName::Null => PrimitiveResultKind::Null,
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Scalar,
    String,
    ViewString,
    Task,
    Result,
    ScalarResult(PrimitiveResultKind),
    ResultString,
}

impl ValueKind {
    fn is_scalar_result(self) -> bool {
        matches!(self, Self::Result | Self::ScalarResult(_))
    }
}

#[derive(Clone, Copy)]
struct Binding<'a> {
    name: &'a str,
    kind: ValueKind,
}

fn binding_kind(bound: &[Binding<'_>], name: &str) -> Option<ValueKind> {
    bound
        .iter()
        .rev()
        .find_map(|binding| (binding.name == name).then_some(binding.kind))
}

fn string_length_call<'a>(expr: &'a Expr, bound: &[Binding<'_>]) -> Option<(&'a str, bool, Span)> {
    let ExprKind::Call { callee, args } = &expr.kind else {
        return None;
    };
    let ExprKind::Field { target, name } = &callee.kind else {
        return None;
    };
    let ExprKind::Variable(symbol) = &target.kind else {
        return None;
    };
    if !args.is_empty()
        || !matches!(
            binding_kind(bound, symbol),
            Some(ValueKind::String | ValueKind::ViewString)
        )
    {
        return None;
    }
    let byte_len = match name.as_str() {
        "len" => false,
        "byte_len" => true,
        _ => return None,
    };
    Some((symbol, byte_len, target.span))
}

fn borrow_string_variable<'a>(expr: &'a Expr, bound: &[Binding<'_>]) -> Admission<&'a str> {
    let ExprKind::Variable(name) = &expr.kind else {
        return Err(AdmissionError::Unsupported(expr.span));
    };
    if !matches!(
        binding_kind(bound, name),
        Some(ValueKind::String | ValueKind::ViewString)
    ) {
        return Err(AdmissionError::Unsupported(expr.span));
    }
    Ok(name)
}

fn selected_string_result_clone_target<'a>(
    expr: &'a Expr,
    bound: &[Binding<'_>],
) -> Option<&'a Expr> {
    let ExprKind::Call { callee, args } = &expr.kind else {
        return None;
    };
    let ExprKind::Field { target, name } = &callee.kind else {
        return None;
    };
    let ExprKind::Variable(symbol) = &target.kind else {
        return None;
    };
    if name != "clone"
        || !args.is_empty()
        || binding_kind(bound, symbol) != Some(ValueKind::ResultString)
    {
        return None;
    }
    Some(target)
}

fn mixed_string_parameter_modes(function: &FnDecl) -> bool {
    let has_view = function
        .params
        .iter()
        .any(|param| param.mode == ParamMode::View && string_type(&param.ty));
    has_view
        && function
            .params
            .iter()
            .any(|param| param.mode == ParamMode::Owned && string_type(&param.ty))
}

fn borrowed_string_arguments(function: &FnDecl, args: &[Expr], span: Span) -> Admission<()> {
    if mixed_string_parameter_modes(function) {
        // This first slice has no additional alias scanner. Mixed String
        // move/loan calls stay whole-tree legacy, even for distinct roots.
        return Err(AdmissionError::Unsupported(span));
    }
    if args.len() != function.params.len() {
        return Err(AdmissionError::Unsupported(span));
    }
    Ok(())
}

// This is a bounded syntax projection, not another type checker. Child
// expressions are validated by the existing flat visit. Knowing their root
// kind is enough to keep Task reads on the move path and Result locals out of
// this slice, without recursively walking or cloning the expression tree.
fn expression_kind(
    expr: &Expr,
    source: &HashMap<String, FnDecl>,
    local: Option<LocalContext<'_>>,
    bound: &[Binding<'_>],
) -> Admission<ValueKind> {
    Ok(match &expr.kind {
        ExprKind::Literal(
            Literal::Int(_) | Literal::Float(_) | Literal::Bool(_) | Literal::Null,
        )
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::TryUnwrap { .. } => ValueKind::Scalar,
        ExprKind::Variable(name) => binding_kind(bound, name).unwrap_or(ValueKind::Scalar),
        ExprKind::Await(_) => ValueKind::Result,
        ExprKind::Call { callee, .. } => {
            if selected_string_result_clone_target(expr, bound).is_some() {
                return Ok(ValueKind::ResultString);
            }
            if string_length_call(expr, bound).is_some() {
                return Ok(ValueKind::Scalar);
            }
            let ExprKind::Variable(name) = &callee.kind else {
                return Err(AdmissionError::Unsupported(callee.span));
            };
            if let Some(local) = local.filter(|local| local.name == name.as_str()) {
                if local.is_async {
                    ValueKind::Task
                } else {
                    ValueKind::Scalar
                }
            } else if let Some(function) = source.get(name) {
                if function.is_async {
                    ValueKind::Task
                } else if string_result_type(&function.return_type) {
                    ValueKind::ResultString
                } else if let Some(kind) = primitive_result_kind(&function.return_type) {
                    ValueKind::ScalarResult(kind)
                } else if scalar_type(&function.return_type) {
                    ValueKind::Scalar
                } else if string_type(&function.return_type) {
                    ValueKind::String
                } else {
                    return Err(AdmissionError::Unsupported(expr.span));
                }
            } else {
                return Err(AdmissionError::Unsupported(callee.span));
            }
        }
        _ => return Err(AdmissionError::Unsupported(expr.span)),
    })
}

fn expect_kind(actual: ValueKind, expected: ValueKind, span: Span) -> Admission<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(AdmissionError::Unsupported(span))
    }
}

fn expect_scalar_result(actual: ValueKind, span: Span) -> Admission<()> {
    if actual.is_scalar_result() {
        Ok(())
    } else {
        Err(AdmissionError::Unsupported(span))
    }
}

fn is_ok_call(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Call { callee, .. }
        if matches!(&callee.kind, ExprKind::Variable(name) if name == "ok"))
}

fn is_err_call(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Call { callee, .. }
        if matches!(&callee.kind, ExprKind::Variable(name) if name == "err"))
}

fn terminal_builtin_argument<'a>(
    expr: &'a Expr,
    source: &HashMap<String, FnDecl>,
    shadowed: bool,
    builtin: &str,
) -> Admission<&'a Expr> {
    let ExprKind::Call { callee, args } = &expr.kind else {
        return Err(AdmissionError::Unsupported(expr.span));
    };
    if !matches!(&callee.kind, ExprKind::Variable(name) if name == builtin)
        || args.len() != 1
        || source.contains_key(builtin)
        || shadowed
    {
        return Err(AdmissionError::Unsupported(expr.span));
    }
    Ok(&args[0])
}

#[derive(Clone, Copy)]
struct ExpressionMode {
    consuming: bool,
    // A primitive argument beside a String loan cannot contain any call in
    // this slice. The flag travels through the existing bounded expression
    // visits, so nested move/mutation calls are rejected before body effects.
    forbid_call: bool,
    // Only the root expression of a charged owned Result terminal relay.
    // Arguments and nested operands always clear this permission.
    terminal_result: bool,
    // Only a selected scalar Result local's complete RHS. A direct await is
    // checked against the current charged async declaration and static callee.
    // Arguments and nested builtin operands clear this permission.
    local_scalar_result: Option<PrimitiveResultKind>,
    // Only a typed String Result local's complete synchronous RHS. This
    // permission never reaches arguments, awaits or nested builtin operands.
    local_string_result: bool,
    // Only the direct Result operand of ? in the new borrowed Result role.
    // A helper or ordinary call site cannot reuse this callee permission.
    unwrap_borrowed_result: bool,
    // Only the static Call immediately consumed by a charged terminal await
    // or the complete RHS of a selected scalar Result local.
    // Nested argument/operand expressions never inherit this permission.
    owned_async_call: bool,
}

enum Visit<'a> {
    Block {
        body: &'a [Stmt],
        id: BlockId,
        next: usize,
        bound_base: usize,
    },
    Expr(&'a Expr, ExprId, ExpressionMode),
    BorrowString(&'a Expr, ExprId),
    Bind(&'a str, bool, ValueKind, Span),
}

struct Builder<'a> {
    source: &'a HashMap<String, FnDecl>,
    local: Option<LocalContext<'a>>,
    arena: CodeArena,
    functions: FrameStack<(FunctionSpec<'a>, FnId)>,
    pending: FrameStack<Visit<'a>>,
    bound: FrameStack<Binding<'a>>,
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
        local: Option<LocalContext<'a>>,
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
        // Validate the requested role before a cached root ID can be reused as
        // a dependency. An async root may be a new AsyncStart dependency, but
        // Result/async roots never become scalar synchronous callees. Only
        // compatible type/charging contracts share a name, so old Result roots
        // cannot impersonate charged roots. Plain scalar helpers retain their
        // compatible Scalar/OwnedNamed reuse.
        spec.validate()?;
        for index in 0..self.arena.functions.len() {
            self.spend(spec.name.len().saturating_add(1), spec.span)?;
            let function = self.arena.functions.get(index).expect("function slot");
            if self.arena.symbol(function.name) == spec.name {
                if (
                    function.is_async,
                    function.result_contract,
                    function.owned_scalar_result,
                ) != spec.cache_contract()
                {
                    return Err(AdmissionError::Unsupported(spec.span));
                }
                return Ok(FnId(index));
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
                is_async: spec.is_async(),
                owned_async_string: spec.is_claimed_async_owned_result_string(),
                owned_scalar_result: spec.owned_scalar_result_kind(),
                result_contract: spec.cache_contract().1,
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

    fn expression(&mut self, expr: &'a Expr, consuming: bool) -> Admission<ExprId> {
        self.expression_with_mode(
            expr,
            ExpressionMode {
                consuming,
                forbid_call: false,
                terminal_result: false,
                local_scalar_result: None,
                local_string_result: false,
                unwrap_borrowed_result: false,
                owned_async_call: false,
            },
        )
    }

    fn expression_with_mode(&mut self, expr: &'a Expr, mode: ExpressionMode) -> Admission<ExprId> {
        let id = self
            .expressions(1, expr.span)?
            .get(0)
            .expect("one expression");
        reserve_field!(self, pending, 1, expr.span);
        push(&mut self.pending, Visit::Expr(expr, id, mode));
        Ok(id)
    }

    fn terminal_ok(
        &mut self,
        expr: &'a Expr,
        owned_string: bool,
        borrowed_result: bool,
        owned_scalar: Option<PrimitiveResultKind>,
    ) -> Admission<ExprId> {
        let shadowed = self.is_bound("ok")
            || self
                .local
                .is_some_and(|local| local.captures.contains("ok"));
        let argument = terminal_builtin_argument(expr, self.source, shadowed, "ok")?;
        expect_kind(
            self.kind(argument)?,
            if owned_string {
                ValueKind::String
            } else {
                ValueKind::Scalar
            },
            argument.span,
        )?;
        // This direct lowering replaces the ordinary visit of the ok Call;
        // charge that syntax step without storing a second visit buffer.
        self.spend(1, expr.span)?;
        let id = self
            .expressions(1, expr.span)?
            .get(0)
            .expect("ok expression");
        let span = self.span(expr.span)?;
        let operand = self.expression(argument, true)?;
        *self
            .arena
            .expressions
            .get_mut(id.0)
            .expect("terminal ok expression slot") = CodeExpr {
            kind: if let Some(kind) = owned_scalar {
                ExprCode::TerminalOwnedScalarOk(operand, kind)
            } else if borrowed_result {
                ExprCode::TerminalBorrowedOk(operand)
            } else {
                ExprCode::TerminalOk(operand, owned_string)
            },
            span,
        };
        Ok(id)
    }

    fn terminal_err(&mut self, expr: &'a Expr) -> Admission<ExprId> {
        let shadowed = self.is_bound("err")
            || self
                .local
                .is_some_and(|local| local.captures.contains("err"));
        let argument = terminal_builtin_argument(expr, self.source, shadowed, "err")?;
        expect_kind(self.kind(argument)?, ValueKind::String, argument.span)?;
        self.spend(1, expr.span)?;
        let id = self
            .expressions(1, expr.span)?
            .get(0)
            .expect("err expression");
        let span = self.span(expr.span)?;
        let operand = self.expression(argument, true)?;
        *self
            .arena
            .expressions
            .get_mut(id.0)
            .expect("terminal err expression slot") = CodeExpr {
            kind: ExprCode::TerminalErr(operand),
            span,
        };
        Ok(id)
    }

    fn local_string_result_expression(
        &mut self,
        expr: &'a Expr,
        spec: FunctionSpec<'a>,
    ) -> Admission<ExprId> {
        spec.check_string_local(expr.span)?;
        if is_ok_call(expr) {
            self.terminal_ok(expr, true, false, None)
        } else if is_err_call(expr) {
            self.terminal_err(expr)
        } else {
            expect_kind(self.kind(expr)?, ValueKind::ResultString, expr.span)?;
            self.expression_with_mode(
                expr,
                ExpressionMode {
                    consuming: true,
                    forbid_call: false,
                    terminal_result: false,
                    local_scalar_result: None,
                    local_string_result: true,
                    unwrap_borrowed_result: false,
                    owned_async_call: false,
                },
            )
        }
    }

    fn local_scalar_result_expression(
        &mut self,
        expr: &'a Expr,
        kind: PrimitiveResultKind,
        spec: FunctionSpec<'a>,
    ) -> Admission<ExprId> {
        if is_ok_call(expr) {
            self.terminal_ok(expr, false, false, Some(kind))
        } else if is_err_call(expr) {
            self.terminal_err(expr)
        } else {
            if let ExprKind::Await(task) = &expr.kind {
                if !spec.is_claimed_async_owned_result() {
                    return Err(AdmissionError::Unsupported(expr.span));
                }
                spec.check_scalar_local_kind(kind, expr.span)?;
                spec.check_terminal_owned_await(task, self.source)?;
            } else {
                expect_kind(self.kind(expr)?, ValueKind::ScalarResult(kind), expr.span)?;
            }
            self.expression_with_mode(
                expr,
                ExpressionMode {
                    consuming: true,
                    forbid_call: false,
                    terminal_result: false,
                    local_scalar_result: Some(kind),
                    local_string_result: false,
                    unwrap_borrowed_result: false,
                    owned_async_call: false,
                },
            )
        }
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

    fn bind(&mut self, name: &'a str, kind: ValueKind, span: Span) -> Admission<()> {
        reserve_field!(self, bound, 1, span);
        push(&mut self.bound, Binding { name, kind });
        Ok(())
    }

    fn is_bound(&self, name: &str) -> bool {
        binding_kind(self.bound.as_slice(), name).is_some()
    }

    fn kind(&self, expr: &Expr) -> Admission<ValueKind> {
        expression_kind(expr, self.source, self.local, self.bound.as_slice())
    }

    fn check_free(&self, name: &str, span: Span) -> Admission<()> {
        if let Some(local) = self.local {
            if !self.is_bound(name)
                && (self.source.contains_key(name)
                    || (local.captures.contains(name) && !local.stable_capture(name)))
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
            self.bind(spec.name, ValueKind::Scalar, spec.span)?;
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
                let (name, span, kind) = match spec.params {
                    Parameters::Named(params) => {
                        let param = &params[index];
                        (param.name.as_str(), param.span, spec.parameter_kind(param)?)
                    }
                    Parameters::Local(params) => {
                        (params[index].as_str(), spec.span, ValueKind::Scalar)
                    }
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
                    owned_string: kind == ValueKind::String,
                    view_string: kind == ValueKind::ViewString,
                };
                self.bind(name, kind, span)?;
            }
            let body = self.block(spec.body, 0)?;
            let function = self.arena.functions.get_mut(id.0).expect("function slot");
            function.params = ParamRange { start, len: count };
            function.body = body;
            while let Some(visit) = self.pending.pop() {
                self.spend(1, spec.span)?;
                self.visit(visit, spec)?;
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

    fn visit(&mut self, visit: Visit<'a>, spec: FunctionSpec<'a>) -> Admission<()> {
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
                let kind = self.statement(stmt, spec)?;
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
            Visit::Bind(name, assignment, kind, span) => {
                if assignment {
                    self.check_free(name, span)?;
                }
                self.bind(name, kind, span)?;
            }
            Visit::BorrowString(expr, id) => {
                if !spec.borrows_strings() {
                    return Err(AdmissionError::Unsupported(expr.span));
                }
                let name = borrow_string_variable(expr, self.bound.as_slice())?;
                self.check_free(name, expr.span)?;
                let symbol = self.symbol(name, expr.span)?;
                let span = self.span(expr.span)?;
                *self
                    .arena
                    .expressions
                    .get_mut(id.0)
                    .expect("borrow argument expression slot") = CodeExpr {
                    kind: ExprCode::StringBorrow(symbol),
                    span,
                };
            }
            Visit::Expr(expr, id, mode) => {
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
                            || self.local.is_some_and(|local| name == local.name)
                        {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        self.check_free(name, expr.span)?;
                        let symbol = self.symbol(name, expr.span)?;
                        match self.kind(expr)? {
                            ValueKind::Scalar => ExprCode::Variable(symbol),
                            ValueKind::Task => ExprCode::VariableOwning(symbol),
                            ValueKind::String if spec.owns_strings() && mode.consuming => {
                                ExprCode::VariableStringOwning(symbol)
                            }
                            ValueKind::String | ValueKind::ViewString => {
                                return Err(AdmissionError::Unsupported(expr.span))
                            }
                            ValueKind::ResultString if spec.stores_string_result() => {
                                ExprCode::VariableSelectedStringResultClone(symbol)
                            }
                            ValueKind::ScalarResult(kind) if spec.stores_scalar_result() => {
                                spec.check_scalar_local_kind(kind, expr.span)?;
                                ExprCode::VariableSelectedScalarResultClone(symbol)
                            }
                            ValueKind::Result
                            | ValueKind::ScalarResult(_)
                            | ValueKind::ResultString => {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                        }
                    }
                    ExprKind::Unary { op, expr } => {
                        expect_kind(self.kind(expr)?, ValueKind::Scalar, expr.span)?;
                        ExprCode::Unary(
                            *op,
                            self.expression_with_mode(
                                expr,
                                ExpressionMode {
                                    consuming: false,
                                    ..mode
                                },
                            )?,
                        )
                    }
                    ExprKind::Binary { left, op, right } => {
                        expect_kind(self.kind(left)?, ValueKind::Scalar, left.span)?;
                        expect_kind(self.kind(right)?, ValueKind::Scalar, right.span)?;
                        let operand_mode = ExpressionMode {
                            consuming: false,
                            ..mode
                        };
                        let right = self.expression_with_mode(right, operand_mode)?;
                        let left = self.expression_with_mode(left, operand_mode)?;
                        ExprCode::Binary(left, *op, right)
                    }
                    ExprKind::Await(task) => {
                        expect_kind(self.kind(task)?, ValueKind::Task, task.span)?;
                        if spec.is_claimed_async_owned_result() {
                            let selected_local = spec.stores_scalar_result()
                                && mode.local_scalar_result == spec.owned_scalar_result_kind();
                            if !(mode.terminal_result || selected_local)
                                || !matches!(task.kind, ExprKind::Call { .. })
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            spec.check_terminal_owned_await(task, self.source)?;
                            ExprCode::TerminalAwaitOwnedString(self.expression_with_mode(
                                task,
                                ExpressionMode {
                                    consuming: true,
                                    forbid_call: false,
                                    terminal_result: false,
                                    local_scalar_result: None,
                                    local_string_result: false,
                                    unwrap_borrowed_result: false,
                                    owned_async_call: true,
                                },
                            )?)
                        } else {
                            if !spec.allows_await() {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            ExprCode::Await(self.expression(task, true)?)
                        }
                    }
                    ExprKind::TryUnwrap { expr: inner } => {
                        if spec.is_claimed_async_owned_result() || spec.owns_scalar_result() {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        expect_scalar_result(self.kind(inner)?, inner.span)?;
                        let operand = if spec.borrows_scalar_result() {
                            self.expression_with_mode(
                                inner,
                                ExpressionMode {
                                    consuming: true,
                                    unwrap_borrowed_result: true,
                                    ..mode
                                },
                            )?
                        } else {
                            self.expression(inner, true)?
                        };
                        ExprCode::Propagate(operand)
                    }
                    ExprKind::Call { callee, args } => {
                        if mode.forbid_call {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        if let Some(target) =
                            selected_string_result_clone_target(expr, self.bound.as_slice())
                        {
                            spec.check_string_local(expr.span)?;
                            let target = self.expression(target, false)?;
                            *self
                                .arena
                                .expressions
                                .get_mut(id.0)
                                .expect("clone call expression slot") = CodeExpr {
                                kind: ExprCode::SelectedStringResultCloneCall(target),
                                span,
                            };
                            return Ok(());
                        }
                        if self.kind(expr)? == ValueKind::ResultString
                            && !(spec.owns_result_string()
                                && (mode.terminal_result
                                    || spec.stores_string_result() && mode.local_string_result))
                        {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        if spec.owns_scalar_result() && self.kind(expr)?.is_scalar_result() {
                            if !(mode.terminal_result
                                || spec.stores_scalar_result()
                                    && mode.local_scalar_result == spec.owned_scalar_result_kind())
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            expect_kind(self.kind(expr)?, spec.return_kind(), expr.span)?;
                        }
                        if spec.borrows_strings()
                            && self.kind(expr)?.is_scalar_result()
                            && !(spec.borrows_scalar_result() && mode.unwrap_borrowed_result)
                        {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        // Owned inputs and their static scalar helpers use
                        // the same real Env read and original length tick as
                        // String loans. This creates no new String or owner.
                        if spec.owns_strings() {
                            if let Some((name, byte_len, target_span)) =
                                string_length_call(expr, self.bound.as_slice())
                            {
                                self.check_free(name, target_span)?;
                                self.spend(1, target_span)?;
                                let symbol = self.symbol(name, target_span)?;
                                let target_span = self.span(target_span)?;
                                *self
                                    .arena
                                    .expressions
                                    .get_mut(id.0)
                                    .expect("length expression slot") = CodeExpr {
                                    kind: ExprCode::StringLength {
                                        symbol,
                                        byte_len,
                                        target_span,
                                    },
                                    span,
                                };
                                return Ok(());
                            }
                        }
                        let ExprKind::Variable(name) = &callee.kind else {
                            return Err(AdmissionError::Unsupported(callee.span));
                        };
                        let function = if let Some(local) = self.local {
                            if name != local.name {
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
                            self.ensure_function(if spec.borrows_strings() {
                                borrowed_string_arguments(function, args, expr.span)?;
                                spec.borrowed_dependency(function)
                            } else if spec.owns_strings() {
                                if function.is_async
                                    && spec.is_claimed_async_owned_result()
                                    && mode.owned_async_call
                                {
                                    spec.owned_async_dependency(function)?
                                } else {
                                    spec.owned_dependency(function)
                                }
                            } else {
                                FunctionSpec::dependency(function)
                            })?
                        };
                        for (index, arg) in args.iter().enumerate() {
                            let expected = if spec.owns_strings() {
                                let declaration =
                                    self.source.get(name).expect("validated named callee");
                                let param = declaration
                                    .params
                                    .get(index)
                                    .ok_or(AdmissionError::Unsupported(expr.span))?;
                                if spec.borrows_strings() && param.mode == ParamMode::View {
                                    borrow_string_variable(arg, self.bound.as_slice())?;
                                    continue;
                                }
                                if string_type(&param.ty) {
                                    ValueKind::String
                                } else {
                                    ValueKind::Scalar
                                }
                            } else {
                                ValueKind::Scalar
                            };
                            expect_kind(self.kind(arg)?, expected, arg.span)?;
                        }
                        if spec.owns_strings()
                            && args.len()
                                != self
                                    .source
                                    .get(name)
                                    .expect("validated named callee")
                                    .params
                                    .len()
                        {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        let range = self.expressions(args.len(), expr.span)?;
                        reserve_field!(self, pending, args.len(), expr.span);
                        let has_view = spec.borrows_strings()
                            && self
                                .source
                                .get(name)
                                .expect("validated named callee")
                                .params
                                .iter()
                                .any(|param| param.mode == ParamMode::View);
                        for (index, arg) in args.iter().enumerate().rev() {
                            let borrowing = spec.borrows_strings()
                                && self
                                    .source
                                    .get(name)
                                    .expect("validated named callee")
                                    .params[index]
                                    .mode
                                    == ParamMode::View;
                            let visit = if borrowing {
                                Visit::BorrowString(arg, range.get(index).expect("argument ID"))
                            } else {
                                Visit::Expr(
                                    arg,
                                    range.get(index).expect("argument ID"),
                                    ExpressionMode {
                                        consuming: true,
                                        forbid_call: has_view,
                                        terminal_result: false,
                                        local_scalar_result: None,
                                        local_string_result: false,
                                        unwrap_borrowed_result: false,
                                        owned_async_call: false,
                                    },
                                )
                            };
                            push(&mut self.pending, visit);
                        }
                        if self
                            .arena
                            .functions
                            .get(function.0)
                            .expect("callee slot")
                            .is_async
                        {
                            if self.local.is_some() {
                                ExprCode::AsyncSelfStart {
                                    function,
                                    args: range,
                                    callee_span: self.span(callee.span)?,
                                }
                            } else {
                                ExprCode::AsyncStart {
                                    function,
                                    args: range,
                                }
                            }
                        } else {
                            ExprCode::Call {
                                function,
                                args: range,
                                callee_span: self.span(callee.span)?,
                            }
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

    fn statement(&mut self, stmt: &'a Stmt, spec: FunctionSpec<'a>) -> Admission<StmtCode> {
        let span = stmt_span(stmt);
        if self.local.is_some_and(|local| matches!(stmt,
            Stmt::VarDecl { name, .. } | Stmt::Assign { name, .. } | Stmt::For { name, .. } if name == local.name)) {
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
                if string_result_type(ty) {
                    spec.check_string_local(span)?;
                    reserve_field!(self, pending, 1, span);
                    push(
                        &mut self.pending,
                        Visit::Bind(name, false, ValueKind::ResultString, span),
                    );
                    let symbol = self.symbol(name, span)?;
                    let value = self.local_string_result_expression(value, spec)?;
                    self.arena.has_actual_string_result_locals = true;
                    return Ok(StmtCode::VarDecl(symbol, value, *mutable));
                }
                let local_kind = primitive_result_kind(ty).or_else(|| {
                    if ty.is_none() {
                        match self.kind(value).ok()? {
                            ValueKind::ScalarResult(kind) => Some(kind),
                            _ => None,
                        }
                    } else {
                        None
                    }
                });
                if let Some(kind) = local_kind {
                    spec.check_scalar_local_kind(kind, span)?;
                    reserve_field!(self, pending, 1, span);
                    push(
                        &mut self.pending,
                        Visit::Bind(name, false, ValueKind::ScalarResult(kind), span),
                    );
                    return Ok(StmtCode::VarDecl(
                        self.symbol(name, span)?,
                        self.local_scalar_result_expression(value, kind, spec)?,
                        *mutable,
                    ));
                }
                if !(scalar_type(ty) || (spec.owns_strings() && string_type(ty))) {
                    return Err(AdmissionError::Unsupported(span));
                }
                let kind = self.kind(value)?;
                if matches!(
                    kind,
                    ValueKind::Result
                        | ValueKind::ScalarResult(_)
                        | ValueKind::ResultString
                        | ValueKind::ViewString
                ) || (kind == ValueKind::String && !spec.owns_strings())
                {
                    // Env::get_owning currently clones a scalar-only Result.
                    // Do not change that legal behavior or add a new implicit
                    // Owned clone in the continuation slice.
                    return Err(AdmissionError::Unsupported(span));
                }
                if string_type(ty) {
                    expect_kind(kind, ValueKind::String, span)?;
                }
                reserve_field!(self, pending, 1, span);
                push(&mut self.pending, Visit::Bind(name, false, kind, span));
                StmtCode::VarDecl(
                    self.symbol(name, span)?,
                    self.expression(value, true)?,
                    *mutable,
                )
            }
            Stmt::Assign { name, value, .. } => {
                if binding_kind(self.bound.as_slice(), name) == Some(ValueKind::ResultString) {
                    spec.check_string_local(span)?;
                    reserve_field!(self, pending, 1, span);
                    push(
                        &mut self.pending,
                        Visit::Bind(name, true, ValueKind::ResultString, span),
                    );
                    let symbol = self.symbol(name, span)?;
                    let value = self.local_string_result_expression(value, spec)?;
                    self.arena.has_actual_string_result_locals = true;
                    return Ok(StmtCode::Assign(symbol, value));
                }
                let local_kind = match binding_kind(self.bound.as_slice(), name) {
                    Some(ValueKind::ScalarResult(kind)) => Some(kind),
                    _ => match self.kind(value).ok() {
                        Some(ValueKind::ScalarResult(kind)) => Some(kind),
                        _ => None,
                    },
                };
                if let Some(kind) = local_kind {
                    spec.check_scalar_local_kind(kind, span)?;
                    reserve_field!(self, pending, 1, span);
                    push(
                        &mut self.pending,
                        Visit::Bind(name, true, ValueKind::ScalarResult(kind), span),
                    );
                    return Ok(StmtCode::Assign(
                        self.symbol(name, span)?,
                        self.local_scalar_result_expression(value, kind, spec)?,
                    ));
                }
                if spec.borrows_strings()
                    && binding_kind(self.bound.as_slice(), name) == Some(ValueKind::ViewString)
                {
                    return Err(AdmissionError::Unsupported(span));
                }
                let kind = self.kind(value)?;
                if matches!(
                    kind,
                    ValueKind::Result
                        | ValueKind::ScalarResult(_)
                        | ValueKind::ResultString
                        | ValueKind::ViewString
                ) || (kind == ValueKind::String && !spec.owns_strings())
                {
                    return Err(AdmissionError::Unsupported(span));
                }
                reserve_field!(self, pending, 1, span);
                push(&mut self.pending, Visit::Bind(name, true, kind, span));
                StmtCode::Assign(self.symbol(name, span)?, self.expression(value, true)?)
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                expect_kind(self.kind(condition)?, ValueKind::Scalar, condition.span)?;
                let base = self.bound.len();
                let else_body = self.block(else_branch, base)?;
                let then_body = self.block(then_branch, base)?;
                StmtCode::If(self.expression(condition, false)?, then_body, else_body)
            }
            Stmt::While {
                condition, body, ..
            } => {
                expect_kind(self.kind(condition)?, ValueKind::Scalar, condition.span)?;
                let body = self.block(body, self.bound.len())?;
                StmtCode::While(self.expression(condition, false)?, body)
            }
            Stmt::For {
                name,
                iterable,
                body,
                ..
            } => {
                expect_kind(self.kind(iterable)?, ValueKind::Scalar, iterable.span)?;
                // The iterator exists only in the body, never the iterable.
                // Bind is deferred until that iterable has been inspected.
                let base = self.bound.len();
                let body = self.block(body, base)?;
                reserve_field!(self, pending, 1, span);
                push(
                    &mut self.pending,
                    Visit::Bind(name, false, ValueKind::Scalar, span),
                );
                StmtCode::For(
                    self.symbol(name, span)?,
                    self.expression(iterable, false)?,
                    body,
                )
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
                if spec.borrows_scalar_result() || catch_name.is_some() || !catch_body.is_empty() {
                    return Err(AdmissionError::Unsupported(span));
                }
                let base = self.bound.len();
                let finally = self.block(finally_body, base)?;
                StmtCode::Try(self.block(body, base)?, finally)
            }
            Stmt::Return { value, .. } => {
                let value = if spec.owns_result_string() || spec.owns_scalar_result() {
                    let value = value.as_ref().ok_or(AdmissionError::Unsupported(span))?;
                    if is_ok_call(value) {
                        Some(self.terminal_ok(
                            value,
                            spec.owns_result_string(),
                            false,
                            spec.owned_scalar_result_kind(),
                        )?)
                    } else if is_err_call(value) {
                        Some(self.terminal_err(value)?)
                    } else {
                        if !(spec.is_claimed_async_owned_result()
                            && matches!(value.kind, ExprKind::Await(_)))
                        {
                            expect_kind(self.kind(value)?, spec.return_kind(), value.span)?;
                        }
                        Some(self.expression_with_mode(
                            value,
                            ExpressionMode {
                                consuming: true,
                                forbid_call: false,
                                terminal_result: true,
                                local_scalar_result: None,
                                local_string_result: false,
                                unwrap_borrowed_result: false,
                                owned_async_call: false,
                            },
                        )?)
                    }
                } else if spec.is_result_root() {
                    let value = value.as_ref().ok_or(AdmissionError::Unsupported(span))?;
                    Some(self.terminal_ok(value, false, spec.borrows_scalar_result(), None)?)
                } else {
                    if let Some(value) = value {
                        expect_kind(self.kind(value)?, spec.return_kind(), value.span)?;
                    }
                    value
                        .as_ref()
                        .map(|value| self.expression(value, true))
                        .transpose()?
                };
                StmtCode::Return(value)
            }
            Stmt::Expr { expr, .. } => StmtCode::Expr(self.expression(expr, false)?),
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
    Expr(&'a Expr, ExpressionMode),
    BorrowString(&'a Expr),
    Bind(&'a str, bool, ValueKind, Span),
}

struct Eligibility<'a> {
    source: &'a HashMap<String, FnDecl>,
    local: Option<LocalContext<'a>>,
    functions: FrameStack<FunctionSpec<'a>>,
    checked: FrameStack<(&'a str, FunctionContract)>,
    pending: FrameStack<Scan<'a>>,
    bound: FrameStack<Binding<'a>>,
    limits: FrameLimits,
    remaining: usize,
}

impl<'a> Eligibility<'a> {
    fn new(
        source: &'a HashMap<String, FnDecl>,
        local: Option<LocalContext<'a>>,
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
        binding_kind(self.bound.as_slice(), name).is_some()
    }
    fn bind(&mut self, name: &'a str, kind: ValueKind, span: Span) -> Admission<()> {
        reserve_field!(self, bound, 1, span);
        push(&mut self.bound, Binding { name, kind });
        Ok(())
    }
    fn kind(&self, expr: &Expr) -> Admission<ValueKind> {
        expression_kind(expr, self.source, self.local, self.bound.as_slice())
    }
    fn free(&self, name: &str, span: Span) -> Admission<()> {
        if let Some(local) = self.local {
            if !self.bound(name)
                && (self.source.contains_key(name)
                    || (local.captures.contains(name) && !local.stable_capture(name)))
            {
                return Err(AdmissionError::Unsupported(span));
            }
        }
        Ok(())
    }
    fn function(&mut self, spec: FunctionSpec<'a>) -> Admission<()> {
        // A previously checked Result root cannot bypass the scalar dependency
        // contract or change its Box charging merely by sharing a name.
        spec.validate()?;
        if let Some((_, contract)) = self
            .checked
            .as_slice()
            .iter()
            .find(|(name, _)| *name == spec.name)
        {
            if *contract != spec.cache_contract() {
                return Err(AdmissionError::Unsupported(spec.span));
            }
            return Ok(());
        }
        reserve_field!(self, checked, 1, spec.span);
        push(&mut self.checked, (spec.name, spec.cache_contract()));
        reserve_field!(self, functions, 1, spec.span);
        push(&mut self.functions, spec);
        Ok(())
    }
    fn expression(&mut self, expr: &'a Expr, consuming: bool) -> Admission<()> {
        self.expression_with_mode(
            expr,
            ExpressionMode {
                consuming,
                forbid_call: false,
                terminal_result: false,
                local_scalar_result: None,
                local_string_result: false,
                unwrap_borrowed_result: false,
                owned_async_call: false,
            },
        )
    }

    fn expression_with_mode(&mut self, expr: &'a Expr, mode: ExpressionMode) -> Admission<()> {
        reserve_field!(self, pending, 1, expr.span);
        push(&mut self.pending, Scan::Expr(expr, mode));
        Ok(())
    }

    fn local_string_result_expression(
        &mut self,
        expr: &'a Expr,
        spec: FunctionSpec<'a>,
    ) -> Admission<()> {
        spec.check_string_local(expr.span)?;
        let builtin = if is_ok_call(expr) {
            Some("ok")
        } else if is_err_call(expr) {
            Some("err")
        } else {
            None
        };
        if let Some(name) = builtin {
            let shadowed = self.bound(name)
                || self
                    .local
                    .is_some_and(|local| local.captures.contains(name));
            let argument = terminal_builtin_argument(expr, self.source, shadowed, name)?;
            expect_kind(self.kind(argument)?, ValueKind::String, argument.span)?;
            self.spend(1, expr.span)?;
            self.expression(argument, true)
        } else {
            expect_kind(self.kind(expr)?, ValueKind::ResultString, expr.span)?;
            self.expression_with_mode(
                expr,
                ExpressionMode {
                    consuming: true,
                    forbid_call: false,
                    terminal_result: false,
                    local_scalar_result: None,
                    local_string_result: true,
                    unwrap_borrowed_result: false,
                    owned_async_call: false,
                },
            )
        }
    }

    fn local_scalar_result_expression(
        &mut self,
        expr: &'a Expr,
        kind: PrimitiveResultKind,
        spec: FunctionSpec<'a>,
    ) -> Admission<()> {
        let builtin = if is_ok_call(expr) {
            Some(("ok", ValueKind::Scalar))
        } else if is_err_call(expr) {
            Some(("err", ValueKind::String))
        } else {
            None
        };
        if let Some((name, expected)) = builtin {
            let shadowed = self.bound(name)
                || self
                    .local
                    .is_some_and(|local| local.captures.contains(name));
            let argument = terminal_builtin_argument(expr, self.source, shadowed, name)?;
            expect_kind(self.kind(argument)?, expected, argument.span)?;
            self.spend(1, expr.span)?;
            self.expression(argument, true)
        } else {
            if let ExprKind::Await(task) = &expr.kind {
                if !spec.is_claimed_async_owned_result() {
                    return Err(AdmissionError::Unsupported(expr.span));
                }
                spec.check_scalar_local_kind(kind, expr.span)?;
                spec.check_terminal_owned_await(task, self.source)?;
            } else {
                expect_kind(self.kind(expr)?, ValueKind::ScalarResult(kind), expr.span)?;
            }
            self.expression_with_mode(
                expr,
                ExpressionMode {
                    consuming: true,
                    forbid_call: false,
                    terminal_result: false,
                    local_scalar_result: Some(kind),
                    local_string_result: false,
                    unwrap_borrowed_result: false,
                    owned_async_call: false,
                },
            )
        }
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
            self.bind(spec.name, ValueKind::Scalar, spec.span)?;
            let count = match spec.params {
                Parameters::Named(params) => params.len(),
                Parameters::Local(params) => params.len(),
            };
            self.spend(count, spec.span)?;
            for index in 0..count {
                let (name, kind) = match spec.params {
                    Parameters::Named(params) => (
                        params[index].name.as_str(),
                        spec.parameter_kind(&params[index])?,
                    ),
                    Parameters::Local(params) => (params[index].as_str(), ValueKind::Scalar),
                };
                if self.local.is_some() && (name == spec.name || self.bound(name)) {
                    return Err(AdmissionError::Unsupported(spec.span));
                }
                self.bind(name, kind, spec.span)?;
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
                        if self.local.is_some_and(|local| matches!(stmt,
                            Stmt::VarDecl { name, .. } | Stmt::Assign { name, .. } | Stmt::For { name, .. } if name == local.name)) {
                            return Err(AdmissionError::Unsupported(span));
                        }
                        match stmt {
                            Stmt::VarDecl {
                                name, ty, value, ..
                            } => {
                                if string_result_type(ty) {
                                    spec.check_string_local(span)?;
                                    reserve_field!(self, pending, 1, span);
                                    push(
                                        &mut self.pending,
                                        Scan::Bind(name, false, ValueKind::ResultString, span),
                                    );
                                    self.local_string_result_expression(value, spec)?;
                                    continue;
                                }
                                let local_kind = primitive_result_kind(ty).or_else(|| {
                                    if ty.is_none() {
                                        match self.kind(value).ok()? {
                                            ValueKind::ScalarResult(kind) => Some(kind),
                                            _ => None,
                                        }
                                    } else {
                                        None
                                    }
                                });
                                if let Some(kind) = local_kind {
                                    spec.check_scalar_local_kind(kind, span)?;
                                    reserve_field!(self, pending, 1, span);
                                    push(
                                        &mut self.pending,
                                        Scan::Bind(
                                            name,
                                            false,
                                            ValueKind::ScalarResult(kind),
                                            span,
                                        ),
                                    );
                                    self.local_scalar_result_expression(value, kind, spec)?;
                                    continue;
                                }
                                if !(scalar_type(ty) || (spec.owns_strings() && string_type(ty))) {
                                    return Err(AdmissionError::Unsupported(span));
                                }
                                let kind = self.kind(value)?;
                                if matches!(
                                    kind,
                                    ValueKind::Result
                                        | ValueKind::ScalarResult(_)
                                        | ValueKind::ResultString
                                        | ValueKind::ViewString
                                ) || (kind == ValueKind::String && !spec.owns_strings())
                                {
                                    return Err(AdmissionError::Unsupported(span));
                                }
                                if string_type(ty) {
                                    expect_kind(kind, ValueKind::String, span)?;
                                }
                                reserve_field!(self, pending, 1, span);
                                push(&mut self.pending, Scan::Bind(name, false, kind, span));
                                self.expression(value, true)?;
                            }
                            Stmt::Assign { name, value, .. } => {
                                if binding_kind(self.bound.as_slice(), name)
                                    == Some(ValueKind::ResultString)
                                {
                                    spec.check_string_local(span)?;
                                    reserve_field!(self, pending, 1, span);
                                    push(
                                        &mut self.pending,
                                        Scan::Bind(name, true, ValueKind::ResultString, span),
                                    );
                                    self.local_string_result_expression(value, spec)?;
                                    continue;
                                }
                                let local_kind = match binding_kind(self.bound.as_slice(), name) {
                                    Some(ValueKind::ScalarResult(kind)) => Some(kind),
                                    _ => match self.kind(value).ok() {
                                        Some(ValueKind::ScalarResult(kind)) => Some(kind),
                                        _ => None,
                                    },
                                };
                                if let Some(kind) = local_kind {
                                    spec.check_scalar_local_kind(kind, span)?;
                                    reserve_field!(self, pending, 1, span);
                                    push(
                                        &mut self.pending,
                                        Scan::Bind(name, true, ValueKind::ScalarResult(kind), span),
                                    );
                                    self.local_scalar_result_expression(value, kind, spec)?;
                                    continue;
                                }
                                if spec.borrows_strings()
                                    && binding_kind(self.bound.as_slice(), name)
                                        == Some(ValueKind::ViewString)
                                {
                                    return Err(AdmissionError::Unsupported(span));
                                }
                                let kind = self.kind(value)?;
                                if matches!(
                                    kind,
                                    ValueKind::Result
                                        | ValueKind::ScalarResult(_)
                                        | ValueKind::ResultString
                                        | ValueKind::ViewString
                                ) || (kind == ValueKind::String && !spec.owns_strings())
                                {
                                    return Err(AdmissionError::Unsupported(span));
                                }
                                reserve_field!(self, pending, 1, span);
                                push(&mut self.pending, Scan::Bind(name, true, kind, span));
                                self.expression(value, true)?;
                            }
                            Stmt::Return { value, .. } => {
                                if spec.owns_result_string() || spec.owns_scalar_result() {
                                    let value =
                                        value.as_ref().ok_or(AdmissionError::Unsupported(span))?;
                                    if is_ok_call(value) {
                                        let shadowed = self.bound("ok");
                                        let argument = terminal_builtin_argument(
                                            value,
                                            self.source,
                                            shadowed,
                                            "ok",
                                        )?;
                                        expect_kind(
                                            self.kind(argument)?,
                                            if spec.owns_result_string() {
                                                ValueKind::String
                                            } else {
                                                ValueKind::Scalar
                                            },
                                            argument.span,
                                        )?;
                                        self.spend(1, value.span)?;
                                        self.expression(argument, true)?;
                                    } else if is_err_call(value) {
                                        let shadowed = self.bound("err")
                                            || self.local.is_some_and(|local| {
                                                local.captures.contains("err")
                                            });
                                        let argument = terminal_builtin_argument(
                                            value,
                                            self.source,
                                            shadowed,
                                            "err",
                                        )?;
                                        expect_kind(
                                            self.kind(argument)?,
                                            ValueKind::String,
                                            argument.span,
                                        )?;
                                        self.spend(1, value.span)?;
                                        self.expression(argument, true)?;
                                    } else {
                                        if !(spec.is_claimed_async_owned_result()
                                            && matches!(value.kind, ExprKind::Await(_)))
                                        {
                                            expect_kind(
                                                self.kind(value)?,
                                                spec.return_kind(),
                                                value.span,
                                            )?;
                                        }
                                        self.expression_with_mode(
                                            value,
                                            ExpressionMode {
                                                consuming: true,
                                                forbid_call: false,
                                                terminal_result: true,
                                                local_scalar_result: None,
                                                local_string_result: false,
                                                unwrap_borrowed_result: false,
                                                owned_async_call: false,
                                            },
                                        )?;
                                    }
                                } else if spec.is_result_root() {
                                    let value =
                                        value.as_ref().ok_or(AdmissionError::Unsupported(span))?;
                                    let shadowed = self.bound("ok")
                                        || self
                                            .local
                                            .is_some_and(|local| local.captures.contains("ok"));
                                    let argument = terminal_builtin_argument(
                                        value,
                                        self.source,
                                        shadowed,
                                        "ok",
                                    )?;
                                    expect_kind(
                                        self.kind(argument)?,
                                        ValueKind::Scalar,
                                        argument.span,
                                    )?;
                                    self.spend(1, value.span)?;
                                    self.expression(argument, true)?;
                                } else if let Some(value) = value {
                                    expect_kind(self.kind(value)?, spec.return_kind(), value.span)?;
                                    self.expression(value, true)?;
                                }
                            }
                            Stmt::Expr { expr, .. } => self.expression(expr, false)?,
                            Stmt::If {
                                condition,
                                then_branch,
                                else_branch,
                                ..
                            } => {
                                expect_kind(
                                    self.kind(condition)?,
                                    ValueKind::Scalar,
                                    condition.span,
                                )?;
                                let base = self.bound.len();
                                self.block(else_branch, base)?;
                                self.block(then_branch, base)?;
                                self.expression(condition, false)?;
                            }
                            Stmt::While {
                                condition, body, ..
                            } => {
                                expect_kind(
                                    self.kind(condition)?,
                                    ValueKind::Scalar,
                                    condition.span,
                                )?;
                                self.block(body, self.bound.len())?;
                                self.expression(condition, false)?;
                            }
                            Stmt::For {
                                name,
                                iterable,
                                body,
                                ..
                            } => {
                                expect_kind(
                                    self.kind(iterable)?,
                                    ValueKind::Scalar,
                                    iterable.span,
                                )?;
                                self.block(body, self.bound.len())?;
                                reserve_field!(self, pending, 1, span);
                                push(
                                    &mut self.pending,
                                    Scan::Bind(name, false, ValueKind::Scalar, span),
                                );
                                self.expression(iterable, false)?;
                            }
                            Stmt::Break { .. } | Stmt::Continue { .. } => {}
                            Stmt::Try {
                                body,
                                catch_name,
                                catch_body,
                                finally_body,
                                ..
                            } => {
                                if spec.borrows_scalar_result()
                                    || catch_name.is_some()
                                    || !catch_body.is_empty()
                                {
                                    return Err(AdmissionError::Unsupported(span));
                                }
                                let base = self.bound.len();
                                self.block(finally_body, base)?;
                                self.block(body, base)?;
                            }
                            _ => return Err(AdmissionError::Unsupported(span)),
                        }
                    }
                    Scan::Bind(name, assignment, kind, span) => {
                        if assignment {
                            self.free(name, span)?;
                        }
                        self.bind(name, kind, span)?;
                    }
                    Scan::BorrowString(expr) => {
                        if !spec.borrows_strings() {
                            return Err(AdmissionError::Unsupported(expr.span));
                        }
                        let name = borrow_string_variable(expr, self.bound.as_slice())?;
                        self.free(name, expr.span)?;
                    }
                    Scan::Expr(expr, mode) => match &expr.kind {
                        ExprKind::Literal(
                            Literal::Int(_) | Literal::Float(_) | Literal::Bool(_) | Literal::Null,
                        ) => {}
                        ExprKind::Variable(name) => {
                            if (self.local.is_none() && self.source.contains_key(name))
                                || self.local.is_some_and(|local| name == local.name)
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            self.free(name, expr.span)?;
                            let kind = self.kind(expr)?;
                            if kind == ValueKind::ResultString && spec.stores_string_result() {
                                continue;
                            }
                            if let ValueKind::ScalarResult(kind) = kind {
                                spec.check_scalar_local_kind(kind, expr.span)?;
                                continue;
                            }
                            if matches!(
                                kind,
                                ValueKind::Result
                                    | ValueKind::ScalarResult(_)
                                    | ValueKind::ResultString
                                    | ValueKind::ViewString
                            ) || (kind == ValueKind::String
                                && !(spec.owns_strings() && mode.consuming))
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                        }
                        ExprKind::Unary { expr, .. } => {
                            expect_kind(self.kind(expr)?, ValueKind::Scalar, expr.span)?;
                            self.expression_with_mode(
                                expr,
                                ExpressionMode {
                                    consuming: false,
                                    ..mode
                                },
                            )?;
                        }
                        ExprKind::Binary { left, right, .. } => {
                            expect_kind(self.kind(left)?, ValueKind::Scalar, left.span)?;
                            expect_kind(self.kind(right)?, ValueKind::Scalar, right.span)?;
                            let operand_mode = ExpressionMode {
                                consuming: false,
                                ..mode
                            };
                            self.expression_with_mode(right, operand_mode)?;
                            self.expression_with_mode(left, operand_mode)?;
                        }
                        ExprKind::Await(task) => {
                            expect_kind(self.kind(task)?, ValueKind::Task, task.span)?;
                            if spec.is_claimed_async_owned_result() {
                                let selected_local = spec.stores_scalar_result()
                                    && mode.local_scalar_result == spec.owned_scalar_result_kind();
                                if !(mode.terminal_result || selected_local)
                                    || !matches!(task.kind, ExprKind::Call { .. })
                                {
                                    return Err(AdmissionError::Unsupported(expr.span));
                                }
                                spec.check_terminal_owned_await(task, self.source)?;
                                self.expression_with_mode(
                                    task,
                                    ExpressionMode {
                                        consuming: true,
                                        forbid_call: false,
                                        terminal_result: false,
                                        local_scalar_result: None,
                                        local_string_result: false,
                                        unwrap_borrowed_result: false,
                                        owned_async_call: true,
                                    },
                                )?;
                            } else {
                                if !spec.allows_await() {
                                    return Err(AdmissionError::Unsupported(expr.span));
                                }
                                self.expression(task, true)?;
                            }
                        }
                        ExprKind::TryUnwrap { expr: inner } => {
                            if spec.is_claimed_async_owned_result() || spec.owns_scalar_result() {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            expect_scalar_result(self.kind(inner)?, inner.span)?;
                            if spec.borrows_scalar_result() {
                                self.expression_with_mode(
                                    inner,
                                    ExpressionMode {
                                        consuming: true,
                                        unwrap_borrowed_result: true,
                                        ..mode
                                    },
                                )?;
                            } else {
                                self.expression(inner, true)?;
                            }
                        }
                        ExprKind::Call { callee, args } => {
                            if mode.forbid_call {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            if let Some(target) =
                                selected_string_result_clone_target(expr, self.bound.as_slice())
                            {
                                spec.check_string_local(expr.span)?;
                                self.expression(target, false)?;
                                continue;
                            }
                            if self.kind(expr)? == ValueKind::ResultString
                                && !(spec.owns_result_string()
                                    && (mode.terminal_result
                                        || spec.stores_string_result() && mode.local_string_result))
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            if spec.owns_scalar_result() && self.kind(expr)?.is_scalar_result() {
                                if !(mode.terminal_result
                                    || spec.stores_scalar_result()
                                        && mode.local_scalar_result
                                            == spec.owned_scalar_result_kind())
                                {
                                    return Err(AdmissionError::Unsupported(expr.span));
                                }
                                expect_kind(self.kind(expr)?, spec.return_kind(), expr.span)?;
                            }
                            if spec.borrows_strings()
                                && self.kind(expr)?.is_scalar_result()
                                && !(spec.borrows_scalar_result() && mode.unwrap_borrowed_result)
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            if spec.owns_strings() {
                                if let Some((name, _, target_span)) =
                                    string_length_call(expr, self.bound.as_slice())
                                {
                                    self.free(name, target_span)?;
                                    self.spend(1, target_span)?;
                                    continue;
                                }
                            }
                            let ExprKind::Variable(name) = &callee.kind else {
                                return Err(AdmissionError::Unsupported(callee.span));
                            };
                            if let Some(local) = self.local {
                                if name != local.name || args.len() != count {
                                    return Err(AdmissionError::Unsupported(expr.span));
                                }
                            } else {
                                let function = self
                                    .source
                                    .get(name)
                                    .ok_or(AdmissionError::Unsupported(callee.span))?;
                                self.function(if spec.borrows_strings() {
                                    borrowed_string_arguments(function, args, expr.span)?;
                                    spec.borrowed_dependency(function)
                                } else if spec.owns_strings() {
                                    if function.is_async
                                        && spec.is_claimed_async_owned_result()
                                        && mode.owned_async_call
                                    {
                                        spec.owned_async_dependency(function)?
                                    } else {
                                        spec.owned_dependency(function)
                                    }
                                } else {
                                    FunctionSpec::dependency(function)
                                })?;
                            }
                            for (index, arg) in args.iter().enumerate() {
                                let expected = if spec.owns_strings() {
                                    let declaration =
                                        self.source.get(name).expect("validated named callee");
                                    let param = declaration
                                        .params
                                        .get(index)
                                        .ok_or(AdmissionError::Unsupported(expr.span))?;
                                    if spec.borrows_strings() && param.mode == ParamMode::View {
                                        borrow_string_variable(arg, self.bound.as_slice())?;
                                        continue;
                                    }
                                    if string_type(&param.ty) {
                                        ValueKind::String
                                    } else {
                                        ValueKind::Scalar
                                    }
                                } else {
                                    ValueKind::Scalar
                                };
                                expect_kind(self.kind(arg)?, expected, arg.span)?;
                            }
                            if spec.owns_strings()
                                && args.len()
                                    != self
                                        .source
                                        .get(name)
                                        .expect("validated named callee")
                                        .params
                                        .len()
                            {
                                return Err(AdmissionError::Unsupported(expr.span));
                            }
                            reserve_field!(self, pending, args.len(), expr.span);
                            let has_view = spec.borrows_strings()
                                && self
                                    .source
                                    .get(name)
                                    .expect("validated named callee")
                                    .params
                                    .iter()
                                    .any(|param| param.mode == ParamMode::View);
                            for (index, arg) in args.iter().enumerate().rev() {
                                let borrowing = spec.borrows_strings()
                                    && self
                                        .source
                                        .get(name)
                                        .expect("validated named callee")
                                        .params[index]
                                        .mode
                                        == ParamMode::View;
                                let scan = if borrowing {
                                    Scan::BorrowString(arg)
                                } else {
                                    Scan::Expr(
                                        arg,
                                        ExpressionMode {
                                            consuming: true,
                                            forbid_call: has_view,
                                            terminal_result: false,
                                            local_scalar_result: None,
                                            local_string_result: false,
                                            unwrap_borrowed_result: false,
                                            owned_async_call: false,
                                        },
                                    )
                                };
                                push(&mut self.pending, scan);
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

#[cfg(test)]
mod typed_result_cache_tests {
    use super::*;

    fn functions(source: &str) -> HashMap<String, FnDecl> {
        let source = format!("{source}\nfn main() {{}}\n");
        let program = Parser::new(Lexer::new(&source).tokenize().unwrap())
            .parse_program()
            .unwrap();
        crate::checker::Checker::new().check(&program).unwrap();
        program
            .items
            .into_iter()
            .map(|item| {
                let Item::Function(function) = item else {
                    panic!("cache fixtures contain only named functions");
                };
                (function.name.clone(), function)
            })
            .collect()
    }

    #[test]
    fn same_name_requires_the_same_typed_result_charging_contract() {
        let source = functions(
            "fn Shared(n: int): int! { return ok(n) }\nfn Scalar(n: int): int { return n }\nasync fn AsyncShared(n: int): int! { return ok(n) }",
        );
        let other = functions("fn Shared(n: int): bool! { return ok(true) }\nasync fn AsyncShared(n: int): bool! { return ok(true) }");
        let source_before = source.clone();
        let result = &source["Shared"];
        let scalar = &source["Scalar"];
        let old = FunctionSpec::named_root(result);
        let owned = FunctionSpec::owned_named(result);
        let borrowed = FunctionSpec::borrowed_string_named(result);
        let other_owned = FunctionSpec::owned_named(&other["Shared"]);
        let plain = FunctionSpec::named(scalar);
        let owned_plain = FunctionSpec::owned_named(scalar);
        let async_result = &source["AsyncShared"];
        let old_async = FunctionSpec::claimed_async_entry(async_result);
        let owned_async = FunctionSpec::claimed_async_owned_scalar_result_entry(async_result);
        let owned_async_dependency = owned_async.owned_async_dependency(async_result).unwrap();
        let other_owned_async =
            FunctionSpec::claimed_async_owned_scalar_result_entry(&other["AsyncShared"]);
        let limits = Limits::default();
        // Complete validation of each real declaration/body happens before
        // testing reuse. No input Value or source binding is mutated and no
        // body is executed by these selection/build operations.
        for spec in [
            old,
            owned,
            borrowed,
            other_owned,
            plain,
            owned_plain,
            old_async,
            owned_async,
            owned_async_dependency,
            other_owned_async,
        ] {
            spec.validate().unwrap();
            assert!(Eligibility::new(&source, None, limits).check(spec).is_ok());
            assert!(Builder::new(&source, None, limits)
                .compile(Some(spec))
                .is_ok());
        }
        for (first, second, compatible) in [
            (old, owned, false),
            (owned, old, false),
            (borrowed, owned, false),
            (owned, borrowed, false),
            (owned, other_owned, false),
            (other_owned, owned, false),
            (plain, owned_plain, true),
            (owned_plain, plain, true),
            (old_async, owned_async, false),
            (owned_async, old_async, false),
            (owned_async, owned_async_dependency, true),
            (owned_async_dependency, owned_async, true),
            (owned_async, other_owned_async, false),
            (other_owned_async, owned_async, false),
        ] {
            let mut builder = Builder::new(&source, None, limits);
            let id = builder.ensure_function(first).unwrap();
            let original = builder.arena.functions.get(id.0).unwrap();
            let contract = (
                original.is_async,
                original.result_contract,
                original.owned_scalar_result,
            );
            let reused = builder.ensure_function(second);
            if compatible {
                assert_eq!(reused.unwrap(), id);
            } else {
                assert!(matches!(reused, Err(AdmissionError::Unsupported(_))));
            }
            assert_eq!(builder.arena.functions.len(), 1);
            assert_eq!(builder.functions.len(), 1);
            let retained = builder.arena.functions.get(id.0).unwrap();
            assert!(
                (
                    retained.is_async,
                    retained.result_contract,
                    retained.owned_scalar_result
                ) == contract
            );

            let mut eligibility = Eligibility::new(&source, None, limits);
            eligibility.function(first).unwrap();
            let reused = eligibility.function(second);
            if compatible {
                reused.unwrap();
            } else {
                assert!(matches!(reused, Err(AdmissionError::Unsupported(_))));
            }
            assert_eq!(eligibility.checked.len(), 1);
            assert_eq!(eligibility.functions.len(), 1);
            assert!(eligibility.checked.get(0).unwrap().1 == contract);
        }
        assert_eq!(source, source_before);

        for (ty, literal) in [
            ("int", "17"),
            ("float", "2.5"),
            ("bool", "true"),
            ("null", "null"),
        ] {
            let locals = functions(&format!(
                "fn Shared(n: int): {ty}! {{ r: {ty}! = ok({literal})\nr\nr = r\nreturn r }}\nasync fn Child(n: int): {ty}! {{ r: {ty}! = ok({literal})\nr\nr = r\nreturn r }}\nasync fn AsyncShared(n: int): {ty}! {{ saved: {ty}! = Shared(n)\nr: {ty}! = await Child(n)\nr\nr = r\nreturn r }}"
            ));
            let before = locals.clone();
            let root =
                FunctionSpec::claimed_async_owned_scalar_result_entry(&locals["AsyncShared"]);
            let dependency = root.owned_async_dependency(&locals["AsyncShared"]).unwrap();
            let old = FunctionSpec::claimed_async_entry(&locals["AsyncShared"]);
            let mut builder = Builder::new(&locals, None, limits);
            let id = builder.ensure_function(root).unwrap();
            assert_eq!(builder.ensure_function(dependency).unwrap(), id);
            assert_eq!(builder.arena.functions.len(), 1);
            assert!(matches!(
                builder.ensure_function(old),
                Err(AdmissionError::Unsupported(_))
            ));
            assert_eq!(builder.arena.functions.len(), 1);
            // Registration has queued only this root. Compile its real body
            // before asserting that the synchronous and async dependencies
            // were discovered, while retaining the cached root identity.
            let compiled = builder.compile(Some(root)).unwrap();
            assert_eq!(compiled.0.functions.len(), 3);
            assert_eq!(compiled.find_function("AsyncShared"), Some(id));
            for name in ["AsyncShared", "Shared", "Child"] {
                let function = compiled.function(compiled.find_function(name).unwrap());
                assert_eq!(
                    function.owned_scalar_result,
                    root.owned_scalar_result_kind()
                );
            }
            let mut eligibility = Eligibility::new(&locals, None, limits);
            eligibility.function(root).unwrap();
            eligibility.function(dependency).unwrap();
            assert_eq!(eligibility.checked.len(), 1);
            assert!(matches!(
                eligibility.function(old),
                Err(AdmissionError::Unsupported(_))
            ));
            assert_eq!(eligibility.checked.len(), 1);
            eligibility.check(root).unwrap();
            assert_eq!(locals, before);
        }
    }
}

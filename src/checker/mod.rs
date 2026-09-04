use std::collections::{BTreeMap, HashMap, HashSet};

use crate::{
    ast::*,
    error::{KuError, KuResult},
    span::Span,
    stdlib::metadata::{self, ArgRule, Signature, TypePattern},
};

const MAX_CHECK_DEPTH: usize = 32;
/// Prefix owned by the compiler's generated C identifiers; see `reject_reserved_name`.
const RESERVED_NAME_PREFIX: &str = "__ku_";
/// Sub-namespaces the import expander synthesizes inside the reserved prefix. These
/// are the only generated names that exist before checking — everything else under
/// `__ku_` is created during lowering or codegen, well after this check runs.
const EXPANDER_PREFIXES: [&str; 2] = ["__ku_import", "__ku_ns"];

#[derive(Debug, Clone, PartialEq)]
enum Type {
    Int,
    Float,
    Bool,
    String,
    Null,
    Array(Box<Type>),
    Result(Box<Type>),
    Task(Box<Type>),
    Union(Vec<Type>),
    Object(HashMap<String, Type>),
    StringMap,
    DynamicObject,
    Struct(String),
    Enum(String),
    /// An opaque owned handle from a C-library binding (e.g. a `pg` connection or
    /// result). The string is the backend's synthetic type id (`__ku_pg_client`). It is
    /// owned (move-tracked, dropped so the backend can close the C resource) and can
    /// expose only checker-recognized receiver methods; it has no user-visible fields.
    Native(String),
    Generic(String),
    Void,
    FunctionValue {
        params: Vec<FunctionValueParam>,
        return_type: Option<Box<Type>>,
        body: Vec<Stmt>,
        /// Checker-local lexical identity for an available function body. Type
        /// annotations carry `None`; concrete top-level/local functions and
        /// closure literals carry a fresh id that survives `Type::clone`.
        body_id: Option<FunctionBodyId>,
        is_async: bool,
    },
    /// A tagged dynamic value read out of a dynamic object (`obj[key]?`,
    /// `get_or`). A first-class type, NOT `Unknown`: only println /
    /// json.stringify / object-array nesting / `==` `!=` / explicit
    /// `.as_int()` / `.as_str()` accept it; arithmetic is rejected.
    KuValue,
    Unknown,
}

/// How a projection path has been moved out. Both states forbid reading the
/// path; they differ only for diagnostics and control-flow merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveMark {
    /// Moved on every path reaching here.
    Moved,
    /// Moved on some control-flow paths but not all; still unreadable until it is
    /// re-initialized.
    MaybeMoved,
}

/// True when `prefix` is a (non-strict) projection prefix of `full`: `[]` is a
/// prefix of everything, `["user"]` is a prefix of `["user", "name"]`.
fn path_is_prefix(prefix: &[String], full: &[String]) -> bool {
    prefix.len() <= full.len() && prefix.iter().zip(full).all(|(a, b)| a == b)
}

/// A movable place: a local variable plus a static struct-field projection path
/// (`user` → `{root: "user", path: []}`, `config.user.name` → `{root: "config",
/// path: ["user", "name"]}`).
#[derive(Debug, Clone)]
struct PlacePath {
    root: String,
    path: Vec<String>,
}

/// What kind of place an expression denotes for move analysis.
enum PlaceClass {
    Movable(PlacePath),
    Index,
    Fresh,
}

/// Render a place as `user.name` / `config.user.name` for diagnostics.
fn place_display(root: &str, path: &[String]) -> String {
    let mut out = root.to_string();
    for segment in path {
        out.push('.');
        out.push_str(segment);
    }
    out
}

fn read_of_moved_error(root: &str, path: &[String], mark: MoveMark, span: Span) -> KuError {
    let place = place_display(root, path);
    let how = match mark {
        MoveMark::Moved => "was moved out",
        MoveMark::MaybeMoved => "may have been moved out on some paths",
    };
    if path.is_empty() {
        KuError::runtime(
            format!("use of moved value '{place}'; it {how} — call '.clone()' when an owned copy is required"),
            span,
        )
    } else {
        KuError::runtime(
            format!("use of moved field '{place}'; it {how} — read it before the move, or '.clone()' it when an owned copy is required"),
            span,
        )
    }
}

fn move_of_moved_error(root: &str, path: &[String], span: Span) -> KuError {
    let place = place_display(root, path);
    if path.is_empty() {
        KuError::runtime(
            format!("use of moved value '{place}'; call '{place}.clone()' before moving when an explicit copy is required"),
            span,
        )
    } else {
        KuError::runtime(
            format!("use of moved field '{place}'; it was already moved out — call '.clone()' when an owned copy is required"),
            span,
        )
    }
}

type BindingId = u64;

/// The binding cells reachable from closures contained in a value. `complete`
/// distinguishes a proven empty dependency set (for example a top-level
/// function) from a value whose provenance the checker could not recover.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosureProvenance {
    dependencies: HashSet<BindingId>,
    complete: bool,
}

impl ClosureProvenance {
    fn empty() -> Self {
        Self {
            dependencies: HashSet::new(),
            complete: true,
        }
    }

    fn unknown() -> Self {
        Self {
            dependencies: HashSet::new(),
            complete: false,
        }
    }

    fn merge(&mut self, other: &Self) {
        self.dependencies.extend(other.dependencies.iter().copied());
        self.complete &= other.complete;
    }
}

#[derive(Debug)]
struct ClosureReturnFlow {
    returned: Option<ClosureProvenance>,
    fallthrough: Option<HashMap<String, ClosureProvenance>>,
    complete: bool,
}

#[derive(Debug, Clone)]
struct ClosureWriteEffect {
    target: BindingId,
    provenance: ClosureProvenance,
}

#[derive(Clone)]
struct ClosureEffectSummary {
    effects: Vec<ClosureWriteEffect>,
    complete: bool,
}

#[derive(Clone)]
struct ClosureEffectEnvironment {
    symbolic: HashMap<String, ClosureProvenance>,
    /// Concrete call-site types for higher-order parameters and aliases. The
    /// annotation alone has no body, while the actual FunctionValue may carry a
    /// checker body id and a captured environment that must be followed for
    /// write-effect analysis.
    types: HashMap<String, Type>,
    locals: HashSet<String>,
}

struct ClosureEffectFlow {
    environment: ClosureEffectEnvironment,
    effects: Vec<ClosureWriteEffect>,
    complete: bool,
    falls_through: bool,
}

#[derive(Clone, Copy)]
struct ClosureBodyView<'a> {
    params: &'a [FunctionValueParam],
    body: &'a [Stmt],
    body_id: FunctionBodyId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClosureProvenanceKey {
    dependencies: Vec<BindingId>,
    complete: bool,
}

impl From<&ClosureProvenance> for ClosureProvenanceKey {
    fn from(provenance: &ClosureProvenance) -> Self {
        let mut dependencies = provenance.dependencies.iter().copied().collect::<Vec<_>>();
        dependencies.sort_unstable();
        Self {
            dependencies,
            complete: provenance.complete,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClosureReturnSummaryKey {
    body_id: FunctionBodyId,
    captured_environment: ClosureProvenanceKey,
    arguments: Vec<ClosureProvenanceKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClosureEffectSummaryKey {
    body_id: FunctionBodyId,
    captured_environment: ClosureProvenanceKey,
    arguments: Vec<ClosureProvenanceKey>,
    /// Provenance can be identical for two function values with different
    /// bodies (for example Discard and a setter). Keep their summaries apart.
    argument_bodies: Vec<Option<FunctionBodyId>>,
}

// Keep this below the default Windows test-thread stack's practical recursive
// depth. Cache hits make ordinary DAGs near-linear; the budget converts a deep
// unique chain to conservative provenance before Rust call-stack exhaustion.
const MAX_CLOSURE_SUMMARY_STATES: usize = 96;

struct ClosureSummaryContext {
    active_bodies: HashSet<FunctionBodyId>,
    return_cache: HashMap<ClosureReturnSummaryKey, ClosureProvenance>,
    active_effect_bodies: HashSet<FunctionBodyId>,
    effect_cache: HashMap<ClosureEffectSummaryKey, ClosureEffectSummary>,
    remaining_states: usize,
}

impl ClosureSummaryContext {
    fn new() -> Self {
        Self {
            active_bodies: HashSet::new(),
            return_cache: HashMap::new(),
            active_effect_bodies: HashSet::new(),
            effect_cache: HashMap::new(),
            remaining_states: MAX_CLOSURE_SUMMARY_STATES,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct VarType {
    binding_id: BindingId,
    ty: Type,
    mutable: bool,
    /// Move state at struct-field-path granularity. A key is a projection path
    /// from this variable (`[]` = the whole variable, `["name"]` = the `name`
    /// field, `["user", "name"]` = a nested field). A present key means that path
    /// — and everything under it — has been moved out; an absent path is live.
    /// Array/object index projections are never tracked here (they cannot be
    /// partially moved; the checker requires an explicit `.clone()`).
    moves: BTreeMap<Vec<String>, MoveMark>,
    /// True only for a `catch` error binding. Its checker type is `Type::Object`
    /// (so `fail {...}` can pass an object literal), but the runtime backs it with
    /// a `KuError` struct whose fields the C backend can move-and-clear — unlike a
    /// user object literal of the same shape. This flag distinguishes the two
    /// reliably, so error fields stay movable while user-object fields require an
    /// explicit `.clone()`.
    /// This binding is lowered to a native struct (a caught error, or an HTTP
    /// handler's request), so its fields are individually movable rather than
    /// being hashmap entries the backend cannot move-and-clear.
    struct_backed: bool,
    /// A closure captured this binding, so its value now lives in a shared cell
    /// that the closure reads on every call. Moving it would empty that cell.
    captured: bool,
    /// Dependencies owned by closures currently stored in this binding. These
    /// form a checker-only graph used to reject local-RC cycles before lowering.
    closure_provenance: ClosureProvenance,
}

impl VarType {
    fn live(
        binding_id: BindingId,
        ty: Type,
        mutable: bool,
        closure_provenance: ClosureProvenance,
    ) -> Self {
        Self {
            binding_id,
            ty,
            mutable,
            moves: BTreeMap::new(),
            struct_backed: false,
            captured: false,
            closure_provenance,
        }
    }

    /// The move mark on the whole variable (`[]`), if any. Used for the bare
    /// "use of moved value" diagnostic and for task/closure whole-value checks.
    fn whole_move(&self) -> Option<MoveMark> {
        self.moves.get(&Vec::new()).copied()
    }

    /// The move mark blocking a read of the place at `path`, if any. A move at `q`
    /// blocks reading `p` when the two lie on one root-to-leaf line: `q` is an
    /// ancestor of `p` (a parent was moved) or `p` is an ancestor of `q` (a part
    /// of `p` was moved, so `p` can no longer be used as a whole).
    fn read_block(&self, path: &[String]) -> Option<MoveMark> {
        self.moves.iter().find_map(|(q, mark)| {
            (path_is_prefix(q, path) || path_is_prefix(path, q)).then_some(*mark)
        })
    }

    /// Mark `path` moved. Any finer moves strictly under `path` are subsumed.
    fn mark_moved(&mut self, path: Vec<String>, mark: MoveMark) {
        self.moves
            .retain(|q, _| !(q.len() > path.len() && path_is_prefix(&path, q)));
        self.moves.insert(path, mark);
    }

    /// Re-initialize `path` and everything under it (an assignment to that place).
    fn reinit(&mut self, path: &[String]) {
        self.moves.retain(|q, _| !path_is_prefix(path, q));
    }

    fn any_moved(&self) -> bool {
        !self.moves.is_empty()
    }
}

type MoveScopes = Vec<HashMap<String, VarType>>;

/// Abrupt exits captured while checking one lexical `try`. Recording them at
/// the actual `return` / `fail` / `?` site is important: an `if` deliberately
/// removes diverging branches from its fallthrough join, and an assignment can
/// re-initialize its target after a throwable RHS has already consumed it.
#[derive(Debug)]
struct TryExitCollector {
    /// Only bindings visible before the try body are visible to catch/finally.
    outer_scope_len: usize,
    returns: Vec<MoveScopes>,
    throws: Vec<MoveScopes>,
}

impl TryExitCollector {
    fn new(outer_scope_len: usize) -> Self {
        Self {
            outer_scope_len,
            returns: Vec::new(),
            throws: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TryExitKind {
    Return,
    Throw,
}

#[derive(Debug, Clone, Copy)]
enum TryPathKind {
    Normal,
    Return,
    Throw,
}

#[derive(Debug, Clone, PartialEq)]
struct FunctionValueParam {
    name: String,
    ty: Option<Type>,
}

type FunctionBodyId = u64;

#[derive(Clone, Copy)]
struct FunctionValueBodyRef<'a> {
    params: &'a [FunctionValueParam],
    return_type: Option<&'a Type>,
    body: &'a [Stmt],
    body_id: Option<FunctionBodyId>,
}

#[derive(Debug, Clone)]
struct FunctionType {
    type_params: Vec<String>,
    params: Vec<Type>,
    value_params: Vec<FunctionValueParam>,
    return_type: Option<Type>,
    returns: Type,
    body: Vec<Stmt>,
    body_id: FunctionBodyId,
    is_async: bool,
}

#[derive(Debug, Clone)]
struct StructType {
    fields: HashMap<String, Type>,
}

#[derive(Debug, Clone)]
struct EnumType {
    variants: HashMap<String, Vec<Type>>,
}

pub struct Checker {
    functions: HashMap<String, FunctionType>,
    structs: HashMap<String, StructType>,
    enums: HashMap<String, EnumType>,
    scopes: Vec<HashMap<String, VarType>>,
    current_return: Type,
    check_depth: usize,
    recoverable_depth: usize,
    loop_depth: usize,
    /// One collector per enclosing loop; each holds the move state captured at
    /// every `break` that exits that loop, so the after-loop state can join them
    /// (a value moved on a break path is moved after the loop).
    loop_break_states: Vec<Vec<Vec<HashMap<String, VarType>>>>,
    /// Move state captured at each `continue`. A `continue` jumps to the top of the
    /// iteration, so unlike a `break` its moves belong to the loop-top join, not to
    /// the state after the loop.
    loop_continue_states: Vec<Vec<Vec<HashMap<String, VarType>>>>,
    template_mode: bool,
    async_depth: usize,
    readonly_capture: Option<ReadonlyCapture>,
    std_modules: HashSet<String>,
    function_value_inference_stack: Vec<FunctionBodyId>,
    /// Bodies currently re-audited below an outer read-only execution boundary.
    /// This guard is independent from return inference because annotated direct
    /// and mutually recursive FunctionValues must terminate too.
    readonly_function_body_stack: Vec<FunctionBodyId>,
    next_function_body_id: FunctionBodyId,
    next_binding_id: BindingId,
    /// Lexical binding identities visible when a concrete local function or
    /// closure body was created. Effect summaries use this table to distinguish
    /// an outer-cell write from an implicit/local binding, even after aliases or
    /// same-name shadowing appear at the call site.
    function_body_outer_bindings: HashMap<FunctionBodyId, HashMap<String, BindingId>>,
    /// Stage 6c-str: scope-index boundaries of the closure bodies currently being
    /// checked (one per nesting level). Moving an owned value found in a scope
    /// below the active boundary out of the closure is rejected (E0904).
    closure_capture_boundaries: Vec<usize>,
    /// One frame per active lexical `try`. Only the innermost frame records an
    /// exit; after its finally is checked, still-pending exits are forwarded to
    /// the parent so nested try/finally chains preserve execution order.
    try_exit_collectors: Vec<TryExitCollector>,
}

impl Checker {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            scopes: vec![HashMap::new()],
            current_return: Type::Void,
            check_depth: 0,
            recoverable_depth: 0,
            loop_depth: 0,
            loop_break_states: Vec::new(),
            loop_continue_states: Vec::new(),
            template_mode: false,
            async_depth: 0,
            readonly_capture: None,
            std_modules: HashSet::new(),
            function_value_inference_stack: Vec::new(),
            readonly_function_body_stack: Vec::new(),
            next_function_body_id: 1,
            next_binding_id: 1,
            function_body_outer_bindings: HashMap::new(),
            closure_capture_boundaries: Vec::new(),
            try_exit_collectors: Vec::new(),
        }
    }

    fn fresh_function_body_id(&mut self) -> FunctionBodyId {
        let body_id = self.next_function_body_id;
        self.next_function_body_id = self
            .next_function_body_id
            .checked_add(1)
            .expect("checker function body id space exhausted");
        body_id
    }

    fn fresh_binding_id(&mut self) -> BindingId {
        let binding_id = self.next_binding_id;
        self.next_binding_id = self
            .next_binding_id
            .checked_add(1)
            .expect("checker binding id space exhausted");
        binding_id
    }

    fn visible_binding_ids(&self) -> HashMap<String, BindingId> {
        let mut bindings = HashMap::new();
        for scope in &self.scopes {
            for (name, binding) in scope {
                bindings.insert(name.clone(), binding.binding_id);
            }
        }
        bindings
    }

    fn record_function_body_outer_bindings(
        &mut self,
        body_id: FunctionBodyId,
        captured_names: &HashSet<String>,
    ) {
        let bindings = self
            .visible_binding_ids()
            .into_iter()
            .filter(|(name, _)| captured_names.contains(name))
            .collect();
        self.function_body_outer_bindings.insert(body_id, bindings);
    }

    pub fn check(mut self, program: &Program) -> KuResult<()> {
        let mut top_level_names = HashMap::new();
        for item in &program.items {
            match item {
                Item::Function(function) => {
                    reject_reserved_name(&function.name, function.span)?;
                    for param in &function.params {
                        reject_reserved_name(&param.name, param.span)?;
                    }
                    let is_async = function.is_async;
                    if let Some(previous_async) =
                        top_level_names.insert(function.name.clone(), is_async)
                    {
                        if function.name == "main" && previous_async != is_async {
                            return Err(KuError::runtime(
                                "async fn main() cannot coexist with fn main()",
                                function.span,
                            ));
                        }
                        return Err(KuError::runtime(
                            format!("top-level name '{}' is already defined", function.name),
                            function.span,
                        ));
                    }
                }
                Item::Import(_) => {}
                Item::Struct(decl) => {
                    reject_reserved_name(&decl.name, decl.span)?;
                    for field in &decl.fields {
                        reject_reserved_name(&field.name, decl.span)?;
                    }
                    if top_level_names.insert(decl.name.clone(), false).is_some() {
                        return Err(KuError::runtime(
                            format!("top-level name '{}' is already defined", decl.name),
                            decl.span,
                        ));
                    }
                }
                Item::Enum(decl) => {
                    reject_reserved_name(&decl.name, decl.span)?;
                    for variant in &decl.variants {
                        reject_reserved_name(&variant.name, decl.span)?;
                        for field in &variant.fields {
                            reject_reserved_name(&field.name, decl.span)?;
                        }
                    }
                    if top_level_names.insert(decl.name.clone(), false).is_some() {
                        return Err(KuError::runtime(
                            format!("top-level name '{}' is already defined", decl.name),
                            decl.span,
                        ));
                    }
                }
                Item::Module(decl) => {
                    if let Some(name) = decl.name.strip_prefix("std:") {
                        self.std_modules.insert(name.to_string());
                        continue;
                    }
                    if top_level_names.insert(decl.name.clone(), false).is_some() {
                        return Err(KuError::runtime(
                            format!("top-level name '{}' is already defined", decl.name),
                            decl.span,
                        ));
                    }
                }
            }
        }

        for item in &program.items {
            match item {
                Item::Struct(decl) => self.collect_struct(decl)?,
                Item::Enum(decl) => self.collect_enum(decl)?,
                Item::Function(_) | Item::Module(_) | Item::Import(_) => {}
            }
        }

        for item in &program.items {
            if let Item::Function(function) = item {
                let body_id = self.fresh_function_body_id();
                self.functions.insert(
                    function.name.clone(),
                    FunctionType {
                        type_params: function.type_params.clone(),
                        params: function
                            .params
                            .iter()
                            .map(|p| {
                                self.resolve_optional_type_name_with_generics(
                                    &p.ty,
                                    p.span,
                                    &function.type_params,
                                )
                            })
                            .collect::<KuResult<Vec<_>>>()?,
                        value_params: function
                            .params
                            .iter()
                            .map(|p| {
                                Ok(FunctionValueParam {
                                    name: p.name.clone(),
                                    ty: p
                                        .ty
                                        .as_ref()
                                        .map(|ty| {
                                            self.resolve_type_name_with_generics(
                                                ty,
                                                p.span,
                                                &function.type_params,
                                            )
                                        })
                                        .transpose()?,
                                })
                            })
                            .collect::<KuResult<Vec<_>>>()?,
                        return_type: function
                            .return_type
                            .as_ref()
                            .map(|ty| {
                                self.resolve_type_name_with_generics(
                                    ty,
                                    function.span,
                                    &function.type_params,
                                )
                            })
                            .transpose()?,
                        returns: function
                            .return_type
                            .as_ref()
                            .map(|ty| {
                                self.resolve_type_name_with_generics(
                                    ty,
                                    function.span,
                                    &function.type_params,
                                )
                            })
                            .transpose()?
                            .unwrap_or(Type::Unknown),
                        body: function.body.clone(),
                        body_id,
                        is_async: function.is_async,
                    },
                );
            }
        }

        if !self.functions.contains_key("main") {
            return Err(KuError::message("missing main function"));
        }
        if let Some(function) = program.items.iter().find_map(|item| match item {
            Item::Function(function) if function.name == "main" => Some(function),
            _ => None,
        }) {
            if !function.params.is_empty() {
                return Err(KuError::runtime(
                    "main function cannot have parameters",
                    function.span,
                ));
            }
        }

        for item in &program.items {
            if let Item::Function(function) = item {
                self.check_function(function)?;
            }
        }
        Ok(())
    }

    fn collect_struct(&mut self, decl: &StructDecl) -> KuResult<()> {
        if self.structs.contains_key(&decl.name) {
            return Err(KuError::runtime(
                format!("struct '{}' is already defined", decl.name),
                decl.span,
            ));
        }
        let mut fields = HashMap::new();
        for field in &decl.fields {
            if fields.contains_key(&field.name) {
                return Err(KuError::runtime(
                    format!("duplicate struct field '{}'", field.name),
                    field.span,
                ));
            }
            fields.insert(
                field.name.clone(),
                self.resolve_required_type_name(&field.ty, field.span)?,
            );
        }
        self.structs
            .insert(decl.name.clone(), StructType { fields });
        Ok(())
    }

    fn collect_enum(&mut self, decl: &EnumDecl) -> KuResult<()> {
        if self.enums.contains_key(&decl.name) {
            return Err(KuError::runtime(
                format!("enum '{}' is already defined", decl.name),
                decl.span,
            ));
        }
        let mut variants = HashMap::new();
        for variant in &decl.variants {
            if variants.contains_key(&variant.name) {
                return Err(KuError::runtime(
                    format!("duplicate enum variant '{}'", variant.name),
                    variant.span,
                ));
            }
            variants.insert(
                variant.name.clone(),
                variant
                    .fields
                    .iter()
                    .map(|p| self.resolve_required_type_name(&p.ty, p.span))
                    .collect::<KuResult<Vec<_>>>()?,
            );
        }
        self.enums.insert(decl.name.clone(), EnumType { variants });
        Ok(())
    }

    fn check_function(&mut self, function: &FnDecl) -> KuResult<()> {
        reject_duplicate_params(function)?;
        let is_async = function.is_async;
        if is_async {
            self.require_async_result_return(function)?;
        }
        let saved_async_depth = self.async_depth;
        self.async_depth = usize::from(is_async);
        self.push_scope();
        let explicit_return = function
            .return_type
            .as_ref()
            .map(|ty| {
                self.resolve_type_name_with_generics(ty, function.span, &function.type_params)
            })
            .transpose()?;
        self.current_return = explicit_return.clone().unwrap_or(Type::Unknown);
        for param in &function.params {
            self.define(
                param.name.clone(),
                self.resolve_optional_type_name_with_generics(
                    &param.ty,
                    param.span,
                    &function.type_params,
                )?,
                false,
                param.span,
            )?;
        }
        let mut inferred_return = Type::Null;
        for stmt in &function.body {
            if let Some(return_type) = self.check_stmt_and_infer_return(stmt)? {
                inferred_return =
                    merge_return_types(&inferred_return, &return_type, stmt_span(stmt))?;
            }
        }
        if let Some(expected) = &explicit_return {
            if expected != &Type::Void && !block_may_return(&function.body) {
                return Err(KuError::runtime(
                    format!(
                        "function '{}' must return {}",
                        function.name,
                        type_name(expected)
                    ),
                    function.span,
                ));
            }
        }
        let resolved_return = explicit_return.unwrap_or(inferred_return);
        if let Some(signature) = self.functions.get_mut(&function.name) {
            signature.returns = resolved_return;
        }
        if self.current_return != Type::Unknown
            && self.current_return != Type::Void
            && !block_may_return(&function.body)
        {
            return Err(KuError::runtime(
                format!(
                    "function '{}' must return {}",
                    function.name,
                    type_name(&self.current_return)
                ),
                function.span,
            ));
        }
        self.pop_scope();
        self.current_return = Type::Void;
        self.async_depth = saved_async_depth;
        Ok(())
    }

    fn resolve_type_name(&self, name: &TypeName, span: Span) -> KuResult<Type> {
        self.resolve_type_name_with_generics(name, span, &[])
    }

    fn resolve_type_name_with_generics(
        &self,
        name: &TypeName,
        span: Span,
        generics: &[String],
    ) -> KuResult<Type> {
        match name {
            TypeName::Int => Ok(Type::Int),
            TypeName::Float => Ok(Type::Float),
            TypeName::Bool => Ok(Type::Bool),
            TypeName::String => Ok(Type::String),
            TypeName::Null => Ok(Type::Null),
            TypeName::Array(inner) => Ok(Type::Array(Box::new(
                self.resolve_type_name_with_generics(inner, span, generics)?,
            ))),
            TypeName::Result(inner) => Ok(Type::Result(Box::new(
                self.resolve_type_name_with_generics(inner, span, generics)?,
            ))),
            TypeName::Function {
                params,
                return_type,
                is_async,
            } => Ok(Type::FunctionValue {
                params: params
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| {
                        Ok(FunctionValueParam {
                            name: format!("arg{index}"),
                            ty: Some(self.resolve_type_name_with_generics(ty, span, generics)?),
                        })
                    })
                    .collect::<KuResult<Vec<_>>>()?,
                return_type: Some(Box::new(self.resolve_type_name_with_generics(
                    return_type,
                    span,
                    generics,
                )?)),
                body: Vec::new(),
                body_id: None,
                is_async: *is_async,
            }),
            TypeName::Union(types) => {
                let mut resolved = Vec::with_capacity(types.len());
                for ty in types {
                    let ty = self.resolve_type_name_with_generics(ty, span, generics)?;
                    if !resolved.iter().any(|existing| type_matches(existing, &ty)) {
                        resolved.push(ty);
                    }
                }
                Ok(Type::Union(resolved))
            }
            TypeName::Custom(name) if generics.contains(name) => Ok(Type::Generic(name.clone())),
            TypeName::Custom(name) if self.structs.contains_key(name) => {
                Ok(Type::Struct(name.clone()))
            }
            TypeName::Custom(name) if self.enums.contains_key(name) => Ok(Type::Enum(name.clone())),
            TypeName::Custom(name) => {
                Err(KuError::runtime(format!("undefined type '{name}'"), span))
            }
        }
    }

    fn resolve_optional_type_name_with_generics(
        &self,
        name: &Option<TypeName>,
        span: Span,
        generics: &[String],
    ) -> KuResult<Type> {
        match name {
            Some(name) => self.resolve_type_name_with_generics(name, span, generics),
            None => Ok(Type::Unknown),
        }
    }

    fn resolve_required_type_name(&self, name: &Option<TypeName>, span: Span) -> KuResult<Type> {
        match name {
            Some(name) => self.resolve_type_name(name, span),
            None => Err(KuError::runtime("expected type name", span)),
        }
    }

    /// Capture the state that exists at an abrupt exit from the innermost try.
    /// This is called only after the exit expression has been consumed, and
    /// therefore before an enclosing assignment can re-initialize its target.
    fn capture_try_exit(&mut self, kind: TryExitKind) {
        let Some(outer_scope_len) = self
            .try_exit_collectors
            .last()
            .map(|collector| collector.outer_scope_len)
        else {
            return;
        };
        let state = self.scopes[..outer_scope_len.min(self.scopes.len())].to_vec();
        let collector = self
            .try_exit_collectors
            .last_mut()
            .expect("try exit collector disappeared while capturing an exit");
        match kind {
            TryExitKind::Return => collector.returns.push(state),
            TryExitKind::Throw => collector.throws.push(state),
        }
    }

    fn take_current_try_exits(&mut self) -> (Vec<MoveScopes>, Vec<MoveScopes>) {
        let collector = self
            .try_exit_collectors
            .last_mut()
            .expect("try exit collection requires an active try");
        (
            std::mem::take(&mut collector.returns),
            std::mem::take(&mut collector.throws),
        )
    }

    /// Forward exits that remain pending after an inner finally to the parent
    /// try. Truncation removes scopes local to the nested construct.
    fn forward_try_exits(&mut self, returns: Vec<MoveScopes>, throws: Vec<MoveScopes>) {
        let Some(parent) = self.try_exit_collectors.last_mut() else {
            return;
        };
        let parent_len = parent.outer_scope_len;
        parent.returns.extend(returns.into_iter().map(|mut state| {
            state.truncate(parent_len);
            state
        }));
        parent.throws.extend(throws.into_iter().map(|mut state| {
            state.truncate(parent_len);
            state
        }));
    }

    fn check_stmt(&mut self, stmt: &Stmt) -> KuResult<()> {
        match stmt {
            Stmt::VarDecl {
                name,
                mutable,
                ty,
                value,
                span,
            } => {
                let declared = ty
                    .as_ref()
                    .map(|ty| self.resolve_type_name(ty, *span))
                    .transpose()?;
                let actual = self.consume_expr_expecting(value, declared.as_ref())?;
                let closure_provenance = if self.type_may_contain_function_value(&actual) {
                    self.expression_closure_provenance(value)
                } else {
                    ClosureProvenance::empty()
                };
                let expected = declared.unwrap_or_else(|| actual.clone());
                if !type_matches(&expected, &actual) {
                    return Err(type_error(*span, &expected, &actual));
                }
                let stored_type = if matches!(expected, Type::FunctionValue { .. })
                    && matches!(actual, Type::FunctionValue { .. })
                {
                    actual.clone()
                } else {
                    expected
                };
                self.define(
                    name.clone(),
                    stored_type,
                    *mutable && !is_constant_name(name),
                    *span,
                )?;
                self.set_closure_provenance(name, closure_provenance, *span)
            }
            Stmt::Assign { name, value, span } => {
                if self.contains(name) {
                    reject_direct_closure_cycle(name, value, *span)?;
                }
                // A closure assigned to an already-declared function-typed
                // variable takes that binding as its expected function type.
                let expected = if self.contains(name) {
                    Some(self.get_allow_moved(name, *span)?.ty)
                } else {
                    None
                };
                let actual = self.consume_expr_expecting(value, expected.as_ref())?;
                let closure_provenance = if self.type_may_contain_function_value(&actual) {
                    self.expression_closure_provenance(value)
                } else {
                    ClosureProvenance::empty()
                };
                if !self.contains(name) {
                    self.define(name.clone(), actual, !is_constant_name(name), *span)?;
                    return self.set_closure_provenance(name, closure_provenance, *span);
                }
                self.reject_readonly_capture_assignment(name, *span)?;
                let binding = self.get_allow_moved(name, *span)?;
                if !binding.mutable {
                    return Err(KuError::runtime(
                        format!("cannot assign to immutable variable '{name}'"),
                        *span,
                    ));
                }
                if !type_matches(&binding.ty, &actual) {
                    return Err(type_error(*span, &binding.ty, &actual));
                }
                self.reject_closure_reference_cycle(name, &closure_provenance, *span)?;
                self.set_closure_provenance(name, closure_provenance, *span)?;
                self.update_function_value_binding_type(name, &actual);
                self.mark_initialized(name);
                Ok(())
            }
            Stmt::AssignTarget {
                target,
                value,
                span,
            } => {
                if let Some(name) = assign_target_root_name(target) {
                    self.reject_readonly_capture_assignment(name, *span)?;
                    if self.contains(name) {
                        reject_direct_closure_cycle(name, value, *span)?;
                    }
                }
                // Assignment evaluates its RHS before resolving the destination
                // (the interpreter and IR both follow this order). Consume first
                // so a later target read observes any move performed by the RHS:
                // `obj["self"] = obj` and `obj[key] = key` must not read a moved
                // receiver/key in native code.
                let actual = self.consume_expr(value)?;
                let expected = self.check_assign_target(target, *span)?;
                if !type_matches(&expected, &actual) {
                    return Err(type_error(*span, &expected, &actual));
                }
                if self.type_may_contain_function_value(&actual) {
                    let closure_provenance = self.expression_closure_provenance(value);
                    if let Some(name) = assign_target_root_name(target) {
                        self.reject_closure_reference_cycle(name, &closure_provenance, *span)?;
                        // Field/index writes update only one unknown slot. Without a
                        // path-sensitive container graph, retain the union of old and
                        // new edges; whole-variable assignment still replaces it and
                        // therefore clears stale edges precisely.
                        self.merge_closure_provenance(name, &closure_provenance, *span)?;
                    }
                }
                // Assigning a moved place re-initializes it (`user.name = x` after
                // `user.name` was moved makes it live again).
                if let Some(place) = self.assign_target_place(target) {
                    self.reinit_place(&place);
                }
                Ok(())
            }
            Stmt::CompoundAssign {
                target,
                op,
                value,
                span,
            } => {
                if let Some(name) = assign_target_root_name(target) {
                    self.reject_readonly_capture_assignment(name, *span)?;
                }
                // Compound assignment has the same RHS-before-target evaluation
                // order as ordinary projection assignment. Checking the target
                // after consuming the RHS lets the normal moved-place diagnostic
                // reject a receiver/key that the RHS just moved.
                let right = self.consume_expr(value)?;
                let left = self.check_assign_target(target, *span)?;
                // `a += b` reads the target before writing it, so the target place
                // must still be live — a compound-assign to a moved field is a
                // use-after-move (`check_assign_target` only checks the base).
                if let Some(place) = self.assign_target_place(target) {
                    self.check_place_readable(&place, *span)?;
                }
                let actual = self.check_binary(*op, &left, &right, *span)?;
                if !type_matches(&left, &actual) {
                    return Err(type_error(*span, &left, &actual));
                }
                if let Some(place) = self.assign_target_place(target) {
                    self.reinit_place(&place);
                }
                Ok(())
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
                for (name, value) in names.iter().zip(values) {
                    if let Some(name) = name {
                        if self.contains(name) {
                            reject_direct_closure_cycle(name, value, *span)?;
                        }
                    }
                }
                let provenances = values
                    .iter()
                    .map(|value| self.expression_closure_provenance(value))
                    .collect::<Vec<_>>();
                let actuals = values
                    .iter()
                    .map(|value| self.consume_expr(value))
                    .collect::<KuResult<Vec<_>>>()?;
                for ((name, actual), provenance) in names.iter().zip(actuals).zip(provenances) {
                    let Some(name) = name else {
                        continue;
                    };
                    self.assign_or_define_destructured(name, actual, provenance, *span)?;
                }
                Ok(())
            }
            Stmt::ObjectDestructureAssign {
                bindings,
                rest,
                value,
                span,
            } => self.check_object_destructure_assign(bindings, rest.as_ref(), value, *span),
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                self.expect_condition(condition, *span)?;
                let before = self.scopes.clone();
                self.check_block(then_branch)?;
                let then_scopes = self.scopes.clone();
                self.scopes = before.clone();
                self.check_block(else_branch)?;
                let else_scopes = self.scopes.clone();
                let then_falls = !block_stops_fallthrough(then_branch);
                let else_falls = !block_stops_fallthrough(else_branch);
                self.scopes =
                    merge_if_scopes(before, then_scopes, else_scopes, then_falls, else_falls);
                Ok(())
            }
            Stmt::While {
                condition,
                body,
                span,
            } => {
                self.expect_condition(condition, *span)?;
                let before = self.scopes.clone();
                let top = self.compute_loop_top(&before, body, None);
                self.scopes = top;
                self.loop_depth += 1;
                self.loop_break_states.push(Vec::new());
                self.loop_continue_states.push(Vec::new());
                let result = self.check_block(body);
                let breaks = self.loop_break_states.pop().unwrap_or_default();
                self.loop_continue_states.pop();
                self.loop_depth -= 1;
                result?;
                self.scopes = self.after_loop_state(before, self.scopes.clone(), breaks);
                Ok(())
            }
            Stmt::For {
                name,
                iterable,
                body,
                span,
            } => {
                let iterable_provenance = self.expression_closure_provenance(iterable);
                let iterable = self.check_expr(iterable)?;
                let element = match iterable {
                    Type::Array(element) => *element,
                    Type::Int => Type::Int,
                    Type::Unknown => Type::Unknown,
                    other => {
                        return Err(KuError::runtime(
                            format!(
                                "type error: for expects array or int but got {}",
                                type_name(&other)
                            ),
                            *span,
                        ));
                    }
                };
                let element_provenance = if self.type_may_contain_function_value(&element) {
                    iterable_provenance
                } else {
                    ClosureProvenance::empty()
                };
                let before = self.scopes.clone();
                let top = self.compute_loop_top(
                    &before,
                    body,
                    Some((name, &element, &element_provenance)),
                );
                self.scopes = top;
                self.push_scope();
                self.loop_depth += 1;
                self.loop_break_states.push(Vec::new());
                self.loop_continue_states.push(Vec::new());
                let result = (|| -> KuResult<()> {
                    self.define(name.clone(), element, true, *span)?;
                    self.set_closure_provenance(name, element_provenance, *span)?;
                    for stmt in body {
                        self.check_stmt(stmt)?;
                    }
                    Ok(())
                })();
                let breaks = self.loop_break_states.pop().unwrap_or_default();
                self.loop_continue_states.pop();
                self.loop_depth -= 1;
                self.pop_scope();
                result?;
                self.scopes = self.after_loop_state(before, self.scopes.clone(), breaks);
                Ok(())
            }
            Stmt::Break { span } => {
                if self.loop_depth == 0 {
                    Err(KuError::runtime("break outside loop", *span))
                } else {
                    // Record the move state here: this state reaches the code after
                    // the loop, so a value moved before the break is moved after it.
                    if let Some(collector) = self.loop_break_states.last_mut() {
                        collector.push(self.scopes.clone());
                    }
                    Ok(())
                }
            }
            Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    Err(KuError::runtime("continue outside loop", *span))
                } else {
                    // A `continue` jumps to the top of the iteration, so the moves
                    // made before it are carried into the next one. `merge_if_scopes`
                    // drops this branch as diverging, so record it here instead.
                    if let Some(collector) = self.loop_continue_states.last_mut() {
                        collector.push(self.scopes.clone());
                    }
                    Ok(())
                }
            }
            Stmt::Function(function) => self.check_local_function(function),
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                span,
            } => {
                let before = self.scopes.clone();
                self.try_exit_collectors
                    .push(TryExitCollector::new(before.len()));
                self.recoverable_depth += 1;
                self.push_scope();
                let mut body_result = Ok(());
                for stmt in body {
                    body_result = self.check_stmt(stmt);
                    if body_result.is_err() {
                        break;
                    }
                    if stmt_stops_fallthrough(stmt) {
                        break;
                    }
                }
                self.pop_scope();
                self.recoverable_depth -= 1;
                if let Err(error) = body_result {
                    self.try_exit_collectors.pop();
                    self.scopes = before;
                    return Err(error);
                }
                let end_state = self.scopes.clone();
                let body_falls = !block_stops_fallthrough(body);
                let (mut pending_returns, body_throws) = self.take_current_try_exits();

                // These are the only paths that can reach code after the try if a
                // finally completes normally. Abrupt paths are kept separately so
                // an `if (...) { move; return }` does not poison ordinary
                // fallthrough, while still being validated by finally.
                let mut normal_paths = Vec::new();
                if body_falls {
                    normal_paths.push(end_state);
                }

                let mut pending_throws = Vec::new();
                if let Some(name) = catch_name {
                    // A catch begins at the join of the actual `fail` / `?` exit
                    // snapshots. In particular, do not merge the raw throw state
                    // into finally after a catch has re-initialized a value.
                    let catch_reachable = !body_throws.is_empty();
                    self.scopes = if catch_reachable {
                        merge_moved_scope_paths(before.clone(), &body_throws)
                    } else {
                        // Keep checking dead catch code for ordinary type/name
                        // errors, matching the checker’s existing behavior.
                        before.clone()
                    };
                    self.push_scope();
                    let catch_result = (|| -> KuResult<()> {
                        self.define(name.clone(), error_type(), false, *span)?;
                        // Flag the binding so its (struct-backed) fields stay movable,
                        // unlike a same-shaped user object literal.
                        if let Some(scope) = self.scopes.last_mut() {
                            if let Some(var) = scope.get_mut(name) {
                                var.struct_backed = true;
                            }
                        }
                        for stmt in catch_body {
                            self.check_stmt(stmt)?;
                            if stmt_stops_fallthrough(stmt) {
                                break;
                            }
                        }
                        Ok(())
                    })();
                    self.pop_scope();
                    let catch_end = self.scopes.clone();
                    if let Err(error) = catch_result {
                        self.try_exit_collectors.pop();
                        self.scopes = before;
                        return Err(error);
                    }
                    let (catch_returns, catch_throws) = self.take_current_try_exits();
                    if catch_reachable {
                        pending_returns.extend(catch_returns);
                        pending_throws = catch_throws;
                        if !block_stops_fallthrough(catch_body) {
                            normal_paths.push(catch_end);
                        }
                    }
                } else {
                    pending_throws = body_throws;
                }

                let normal_input = (!normal_paths.is_empty())
                    .then(|| merge_moved_scope_paths(before.clone(), &normal_paths));

                if finally_body.is_empty() {
                    self.scopes = normal_input.unwrap_or_else(|| before.clone());
                    self.try_exit_collectors.pop();
                    self.forward_try_exits(pending_returns, pending_throws);
                    return Ok(());
                }

                // Native lowering emits a normal, error and return copy of finally.
                // Check the same three path classes independently: every copy must
                // be safe, but a move on an exiting path must not leak into the
                // normal state after the try.
                let mut finally_paths = Vec::new();
                if let Some(state) = normal_input {
                    finally_paths.push((TryPathKind::Normal, state));
                }
                if !pending_returns.is_empty() {
                    finally_paths.push((
                        TryPathKind::Return,
                        merge_moved_scope_paths(before.clone(), &pending_returns),
                    ));
                }
                if !pending_throws.is_empty() {
                    finally_paths.push((
                        TryPathKind::Throw,
                        merge_moved_scope_paths(before.clone(), &pending_throws),
                    ));
                }
                // Unreachable finally code is still checked for type/name errors.
                if finally_paths.is_empty() {
                    finally_paths.push((TryPathKind::Normal, before.clone()));
                }

                let finally_falls = !block_stops_fallthrough(finally_body);
                let mut normal_after = None;
                let mut outgoing_returns = Vec::new();
                let mut outgoing_throws = Vec::new();
                for (path_kind, input) in finally_paths {
                    self.scopes = input;
                    let finally_result = self.check_block(finally_body);
                    if let Err(error) = finally_result {
                        self.try_exit_collectors.pop();
                        self.scopes = before;
                        return Err(error);
                    }
                    let finally_end = self.scopes.clone();
                    let (new_returns, new_throws) = self.take_current_try_exits();
                    outgoing_returns.extend(new_returns);
                    outgoing_throws.extend(new_throws);
                    if finally_falls {
                        match path_kind {
                            TryPathKind::Normal => normal_after = Some(finally_end),
                            TryPathKind::Return => outgoing_returns.push(finally_end),
                            TryPathKind::Throw => outgoing_throws.push(finally_end),
                        }
                    }
                }

                self.scopes = normal_after.unwrap_or_else(|| before.clone());
                self.try_exit_collectors.pop();
                self.forward_try_exits(outgoing_returns, outgoing_throws);
                Ok(())
            }
            Stmt::Fail { value, span } => {
                let actual = self.consume_expr(value)?;
                if actual != Type::String && !matches!(actual, Type::Object(_)) {
                    return Err(type_error(*span, &error_type(), &actual));
                }
                match &self.current_return {
                    Type::Result(_) => {
                        self.capture_try_exit(TryExitKind::Throw);
                        Ok(())
                    }
                    _ if self.recoverable_depth > 0 => {
                        self.capture_try_exit(TryExitKind::Throw);
                        Ok(())
                    }
                    other => Err(KuError::runtime(
                        format!(
                            "fail requires a Result return type or an enclosing try block, got {}",
                            type_name(other)
                        ),
                        *span,
                    )),
                }
            }
            Stmt::Panic { value, .. } => {
                self.check_expr(value)?;
                Ok(())
            }
            Stmt::Return { value, span } => {
                let expected = self.current_return.clone();
                let actual = match value {
                    Some(value) => self.consume_expr_expecting(value, Some(&expected))?,
                    None => Type::Void,
                };
                if !type_matches(&self.current_return, &actual) {
                    return Err(type_error(*span, &self.current_return, &actual));
                }
                self.capture_try_exit(TryExitKind::Return);
                Ok(())
            }
            Stmt::Print { value, .. } => {
                self.check_expr(value)?;
                Ok(())
            }
            Stmt::Expr { expr, .. } => {
                self.check_expr(expr)?;
                Ok(())
            }
        }
    }

    fn check_block(&mut self, body: &[Stmt]) -> KuResult<()> {
        self.push_scope();
        for stmt in body {
            self.check_stmt(stmt)?;
            if stmt_stops_fallthrough(stmt) {
                break;
            }
        }
        self.pop_scope();
        Ok(())
    }

    fn check_object_destructure_assign(
        &mut self,
        bindings: &[ObjectDestructureBinding],
        rest: Option<&ObjectDestructureRest>,
        value: &Expr,
        span: Span,
    ) -> KuResult<()> {
        let source_provenance = self.expression_closure_provenance(value);
        let source_type = self.check_object_destructure_source(value)?;
        let mut consumed = HashSet::new();
        match source_type {
            Type::Object(fields) => {
                for binding in bindings {
                    consumed.insert(binding.field.clone());
                    let actual = match fields.get(&binding.field) {
                        Some(ty) => ty.clone(),
                        None => match &binding.default {
                            Some(default) => self.consume_expr(default)?,
                            None => {
                                return Err(KuError::runtime(
                                    format!("object has no field '{}'", binding.field),
                                    binding.span,
                                ))
                            }
                        },
                    };
                    if let Some(local) = &binding.local {
                        let provenance = if fields.contains_key(&binding.field) {
                            source_provenance.clone()
                        } else {
                            binding
                                .default
                                .as_ref()
                                .map_or_else(ClosureProvenance::empty, |default| {
                                    self.expression_closure_provenance(default)
                                })
                        };
                        self.assign_or_define_destructured(
                            local,
                            actual,
                            provenance,
                            binding.span,
                        )?;
                    }
                }
                if let Some(rest) =
                    rest.and_then(|rest| rest.local.as_ref().map(|name| (rest, name)))
                {
                    let rest_fields = fields
                        .into_iter()
                        .filter(|(name, _)| !consumed.contains(name))
                        .collect::<HashMap<_, _>>();
                    self.assign_or_define_destructured(
                        rest.1,
                        Type::Object(rest_fields),
                        source_provenance.clone(),
                        rest.0.span,
                    )?;
                }
                Ok(())
            }
            Type::DynamicObject | Type::StringMap | Type::Unknown => {
                for binding in bindings {
                    if let Some(default) = &binding.default {
                        self.consume_expr(default)?;
                    }
                    if let Some(local) = &binding.local {
                        let mut provenance = source_provenance.clone();
                        if let Some(default) = &binding.default {
                            provenance.merge(&self.expression_closure_provenance(default));
                        }
                        self.assign_or_define_destructured(
                            local,
                            Type::Unknown,
                            provenance,
                            binding.span,
                        )?;
                    }
                }
                if let Some(rest) =
                    rest.and_then(|rest| rest.local.as_ref().map(|name| (rest, name)))
                {
                    self.assign_or_define_destructured(
                        rest.1,
                        Type::DynamicObject,
                        source_provenance.clone(),
                        rest.0.span,
                    )?;
                }
                Ok(())
            }
            other => Err(KuError::runtime(
                format!(
                    "type error: object destructuring expects object but got {}",
                    type_name(&other)
                ),
                span,
            )),
        }
    }

    fn check_object_destructure_source(&mut self, value: &Expr) -> KuResult<Type> {
        if let ExprKind::Variable(module) = &value.kind {
            if metadata::is_std_module(module)
                && !self.contains(module)
                && self.std_modules.contains(module)
            {
                return std_module_object_type(module, value.span);
            }
        }
        self.consume_expr(value)
    }

    fn assign_or_define_destructured(
        &mut self,
        name: &str,
        actual: Type,
        mut provenance: ClosureProvenance,
        span: Span,
    ) -> KuResult<()> {
        if !self.type_may_contain_function_value(&actual) {
            provenance = ClosureProvenance::empty();
        }
        if !self.contains(name) {
            self.define(name.to_string(), actual, !is_constant_name(name), span)?;
            return self.set_closure_provenance(name, provenance, span);
        }
        self.reject_readonly_capture_assignment(name, span)?;
        let binding = self.get_allow_moved(name, span)?;
        if !binding.mutable {
            return Err(KuError::runtime(
                format!("cannot assign to immutable variable '{name}'"),
                span,
            ));
        }
        if !type_matches(&binding.ty, &actual) {
            return Err(type_error(span, &binding.ty, &actual));
        }
        self.reject_closure_reference_cycle(name, &provenance, span)?;
        self.set_closure_provenance(name, provenance, span)?;
        self.update_function_value_binding_type(name, &actual);
        self.mark_initialized(name);
        Ok(())
    }

    fn check_expr(&mut self, expr: &Expr) -> KuResult<Type> {
        self.check_depth += 1;
        if self.check_depth > MAX_CHECK_DEPTH {
            self.check_depth = self.check_depth.saturating_sub(1);
            return Err(KuError::runtime(
                "maximum check depth exceeded; expression is too deeply nested",
                expr.span,
            ));
        }
        let result = (|| -> KuResult<Type> {
            match &expr.kind {
                ExprKind::Literal(Literal::Int(_)) => Ok(Type::Int),
                ExprKind::Literal(Literal::Float(_)) => Ok(Type::Float),
                ExprKind::Literal(Literal::Bool(_)) => Ok(Type::Bool),
                ExprKind::Literal(Literal::String(_)) => Ok(Type::String),
                ExprKind::Literal(Literal::TemplateString(value)) => {
                    self.check_template_string(value, expr.span)?;
                    Ok(Type::String)
                }
                ExprKind::Literal(Literal::Null) => Ok(Type::Null),
                ExprKind::Variable(name) => {
                    if self.contains(name) {
                        return self.get(name, expr.span).map(|v| v.ty);
                    }
                    if let Some(function) = self.functions.get(name) {
                        return function_value_type(name, function, expr.span);
                    }
                    Err(KuError::runtime(
                        format!("undefined variable '{name}'"),
                        expr.span,
                    ))
                }
                ExprKind::Unary { op, expr: right } => {
                    let right = self.check_expr(right)?;
                    match op {
                        UnaryOp::Negate if right == Type::Int || right == Type::Float => Ok(right),
                        UnaryOp::Not if right == Type::Bool => Ok(Type::Bool),
                        _ => Err(KuError::runtime(
                            format!("invalid unary operation for {}", type_name(&right)),
                            expr.span,
                        )),
                    }
                }
                ExprKind::Binary { left, op, right } => {
                    let left = self.check_expr(left)?;
                    let right = self.check_expr(right)?;
                    self.check_binary(*op, &left, &right, expr.span)
                }
                ExprKind::Call { callee, args } => {
                    if let Some(ty) = self.check_array_map_call(callee, args, expr.span)? {
                        return Ok(ty);
                    }
                    if let Some(ty) = self.check_dotted_builtin_call(callee, args, expr.span)? {
                        return Ok(ty);
                    }
                    if let Some((enum_name, variant)) = enum_variant_path(callee) {
                        if self.enums.contains_key(&enum_name) {
                            return self
                                .check_enum_constructor(&enum_name, &variant, args, expr.span);
                        }
                    }
                    if let Some(ty) = self.check_std_method_call(callee, args, expr.span)? {
                        return Ok(ty);
                    }
                    if let ExprKind::Variable(name) = &callee.kind {
                        if let Some(function) = self.functions.get(name).cloned() {
                            if function.params.len() != args.len() {
                                return Err(KuError::runtime(
                                    format!(
                                        "function '{name}' expects {} arguments but got {}",
                                        function.params.len(),
                                        args.len()
                                    ),
                                    expr.span,
                                ));
                            }
                            let argument_provenance = args
                                .iter()
                                .map(|arg| self.expression_closure_provenance(arg))
                                .collect::<Vec<_>>();
                            let mut generic_bindings = HashMap::new();
                            let mut actual_arg_types = Vec::with_capacity(args.len());
                            for (arg, expected) in args.iter().zip(function.params.iter()) {
                                let actual =
                                    self.consume_arg_expr_expecting(arg, Some(expected))?;
                                if !bind_generic_type(expected, &actual, &mut generic_bindings)
                                    || !type_matches(expected, &actual)
                                {
                                    return Err(type_error(arg.span, expected, &actual));
                                }
                                actual_arg_types.push(actual);
                            }
                            if !function
                                .type_params
                                .iter()
                                .all(|name| generic_bindings.contains_key(name))
                            {
                                return Err(KuError::runtime(
                                    format!("function '{name}' could not infer generic type"),
                                    expr.span,
                                ));
                            }
                            let returns = substitute_generics(&function.returns, &generic_bindings);
                            let effect_params = function
                                .value_params
                                .iter()
                                .map(|param| FunctionValueParam {
                                    name: param.name.clone(),
                                    ty: param
                                        .ty
                                        .as_ref()
                                        .map(|ty| substitute_generics(ty, &generic_bindings)),
                                })
                                .collect::<Vec<_>>();
                            if self.readonly_capture.is_some() {
                                let audited_return = function.return_type.as_ref().map(|ty| {
                                    substitute_generics(ty, &generic_bindings)
                                });
                                self.check_function_value_body(
                                    &effect_params,
                                    audited_return.as_ref(),
                                    &function.body,
                                    Some(function.body_id),
                                    &actual_arg_types,
                                    expr.span,
                                )?;
                            }
                            self.apply_top_level_function_closure_effects(
                                &effect_params,
                                &function.body,
                                function.body_id,
                                &actual_arg_types,
                                &argument_provenance,
                                expr.span,
                            )?;
                            return Ok(if function.is_async {
                                Type::Task(Box::new(returns))
                            } else {
                                returns
                            });
                        }
                        if self.contains(name) {
                            let argument_provenance = args
                                .iter()
                                .map(|arg| self.expression_closure_provenance(arg))
                                .collect::<Vec<_>>();
                            let callee_type = self.get(name, callee.span)?.ty;
                            if let Type::FunctionValue {
                                params,
                                return_type,
                                body,
                                body_id,
                                is_async,
                            } = callee_type
                            {
                                let (returns, actual_arg_types) = self.check_function_value_call(
                                    FunctionValueBodyRef {
                                        params: &params,
                                        return_type: return_type.as_deref(),
                                        body: &body,
                                        body_id,
                                    },
                                    args,
                                    expr.span,
                                    Some(name),
                                )?;
                                if let Some(body_id) = body_id {
                                    self.apply_known_function_closure_effects(
                                        name,
                                        ClosureBodyView {
                                            params: &params,
                                            body: &body,
                                            body_id,
                                        },
                                        &argument_provenance,
                                        &actual_arg_types,
                                        expr.span,
                                    )?;
                                } else {
                                    self.reject_erased_function_value_call_cycle(
                                        name,
                                        &params,
                                        &argument_provenance,
                                        expr.span,
                                    )?;
                                }
                                return Ok(if is_async {
                                    Type::Task(Box::new(returns))
                                } else {
                                    returns
                                });
                            }
                            return Err(KuError::runtime(
                                format!("cannot call {}", type_name(&callee_type)),
                                callee.span,
                            ));
                        }
                        if let Some(ty) = self.check_builtin_call(name, args, expr.span)? {
                            return Ok(ty);
                        }
                        Err(KuError::runtime(
                            format!("undefined function '{name}'"),
                            callee.span,
                        ))
                    } else {
                        if let Some(ty) =
                            self.check_http_service_method_call(callee, args, expr.span)?
                        {
                            return Ok(ty);
                        }
                        if let Some(ty) =
                            self.check_http_config_constructor_call(callee, args, expr.span)?
                        {
                            return Ok(ty);
                        }
                        let argument_provenance = args
                            .iter()
                            .map(|arg| self.expression_closure_provenance(arg))
                            .collect::<Vec<_>>();
                        let callee_root = expr_root_name(callee).map(str::to_string);
                        let callee_type = self.check_expr(callee)?;
                        if let Type::FunctionValue {
                            params,
                            return_type,
                            body,
                            body_id,
                            is_async,
                        } = callee_type
                        {
                            let (returns, actual_arg_types) = self.check_function_value_call(
                                FunctionValueBodyRef {
                                    params: &params,
                                    return_type: return_type.as_deref(),
                                    body: &body,
                                    body_id,
                                },
                                args,
                                expr.span,
                                None,
                            )?;
                            if let Some(callee_root) = callee_root {
                                if let Some(body_id) = body_id {
                                    self.apply_known_function_closure_effects(
                                        &callee_root,
                                        ClosureBodyView {
                                            params: &params,
                                            body: &body,
                                            body_id,
                                        },
                                        &argument_provenance,
                                        &actual_arg_types,
                                        expr.span,
                                    )?;
                                } else {
                                    self.reject_erased_function_value_call_cycle(
                                        &callee_root,
                                        &params,
                                        &argument_provenance,
                                        expr.span,
                                    )?;
                                }
                            }
                            Ok(if is_async {
                                Type::Task(Box::new(returns))
                            } else {
                                returns
                            })
                        } else {
                            Err(KuError::runtime(
                                format!("cannot call {}", type_name(&callee_type)),
                                callee.span,
                            ))
                        }
                    }
                }
                ExprKind::Array(values) => {
                    let mut element_type = Type::Unknown;
                    for value in values {
                        let actual = self.consume_expr(value)?;
                        if element_type == Type::Unknown {
                            element_type = actual;
                        } else if !type_matches(&element_type, &actual) {
                            return Err(type_error(value.span, &element_type, &actual));
                        }
                    }
                    Ok(Type::Array(Box::new(element_type)))
                }
                ExprKind::Index { target, index } => {
                    let target_type = self.check_expr(target)?;
                    let index_type = self.check_expr(index)?;
                    match target_type {
                        Type::Array(element) => {
                            if index_type != Type::Int {
                                return Err(type_error(index.span, &Type::Int, &index_type));
                            }
                            Ok(*element)
                        }
                        Type::String => {
                            if index_type != Type::Int {
                                return Err(type_error(index.span, &Type::Int, &index_type));
                            }
                            Ok(Type::String)
                        }
                        Type::Object(_) => {
                            if index_type != Type::String {
                                return Err(type_error(index.span, &Type::String, &index_type));
                            }
                            Ok(Type::Unknown)
                        }
                        Type::StringMap => {
                            if index_type != Type::String {
                                return Err(type_error(index.span, &Type::String, &index_type));
                            }
                            Ok(Type::String)
                        }
                        Type::DynamicObject => {
                            if index_type != Type::String {
                                return Err(type_error(index.span, &Type::String, &index_type));
                            }
                            Ok(Type::Unknown)
                        }
                        Type::Unknown => Ok(Type::Unknown),
                        other => Err(KuError::runtime(
                            format!("type error: cannot index {}", type_name(&other)),
                            target.span,
                        )),
                    }
                }
                ExprKind::Field { target, name } => {
                    if let ExprKind::Variable(module) = &target.kind {
                        if module == "http"
                            && !self.contains("http")
                            && self.std_modules.contains("http")
                        {
                            match name.as_str() {
                                "service" | "server" => {
                                    return Err(KuError::runtime(
                                        format!(
                                            "std module member 'http.{name}' is a function; call it as 'http.{name}()'"
                                        ),
                                        expr.span,
                                    ))
                                }
                                "status" => return Ok(http_status_type()),
                                "code" => return Ok(http_code_type()),
                                _ => {}
                            }
                        }
                        if let Some(enum_type) = self.enums.get(module) {
                            if let Some(payload) = enum_type.variants.get(name) {
                                if !payload.is_empty() {
                                    return Err(KuError::runtime(
                                        format!(
                                            "enum variant '{module}.{name}' has payload fields; variant constructors are not supported yet"
                                        ),
                                        expr.span,
                                    ));
                                }
                                return Ok(Type::Enum(module.clone()));
                            }
                            return Err(KuError::runtime(
                                format!("enum '{module}' has no variant '{name}'"),
                                expr.span,
                            ));
                        }
                    }
                    let target_type = self.check_expr(target)?;
                    // Reading a field whose place — or an ancestor of it — was moved
                    // out is a use-after-move. Only the exact path or an ancestor
                    // blocks; a moved *descendant* does not, so resolving an
                    // intermediate projection base (`config.user` on the way to
                    // `config.user.age`) stays legal even when a sibling under it
                    // was moved.
                    if let PlaceClass::Movable(place) = self.classify_place(expr) {
                        self.check_place_readable(&place, expr.span)?;
                    }
                    match target_type {
                        Type::Unknown => Ok(Type::Unknown),
                        Type::Struct(struct_name) => {
                            let Some(struct_type) = self.structs.get(&struct_name) else {
                                return Err(KuError::runtime(
                                    format!("undefined struct '{struct_name}'"),
                                    target.span,
                                ));
                            };
                            struct_type.fields.get(name).cloned().ok_or_else(|| {
                                KuError::runtime(
                                    format!("struct '{struct_name}' has no field '{name}'"),
                                    expr.span,
                                )
                            })
                        }
                        Type::Object(fields) => fields.get(name).cloned().ok_or_else(|| {
                            KuError::runtime(format!("object has no field '{name}'"), expr.span)
                        }),
                        Type::StringMap => Ok(Type::String),
                        Type::DynamicObject => Ok(Type::Unknown),
                        Type::Enum(enum_name) => {
                            let Some(enum_type) = self.enums.get(&enum_name) else {
                                return Err(KuError::runtime(
                                    format!("undefined enum '{enum_name}'"),
                                    target.span,
                                ));
                            };
                            if let Some(payload) = enum_type.variants.get(name) {
                                if !payload.is_empty() {
                                    return Err(KuError::runtime(
                                        format!(
                                            "enum variant '{enum_name}.{name}' has payload fields; variant constructors are not supported yet"
                                        ),
                                        expr.span,
                                    ));
                                }
                                Ok(Type::Enum(enum_name))
                            } else {
                                Err(KuError::runtime(
                                    format!("enum '{enum_name}' has no variant '{name}'"),
                                    expr.span,
                                ))
                            }
                        }
                        other => Err(KuError::runtime(
                            format!("type error: {} has no fields", type_name(&other)),
                            target.span,
                        )),
                    }
                }
                ExprKind::OptionalField { target, name } => {
                    let target_type = self.check_expr(target)?;
                    match target_type {
                        Type::Null => Ok(Type::Null),
                        Type::Struct(struct_name) => {
                            let Some(struct_type) = self.structs.get(&struct_name) else {
                                return Err(KuError::runtime(
                                    format!("undefined struct '{struct_name}'"),
                                    target.span,
                                ));
                            };
                            Ok(struct_type.fields.get(name).cloned().unwrap_or(Type::Null))
                        }
                        Type::Object(fields) => Ok(fields.get(name).cloned().unwrap_or(Type::Null)),
                        Type::Unknown => Ok(Type::Unknown),
                        other => Err(KuError::runtime(
                            format!("type error: {} has no fields", type_name(&other)),
                            target.span,
                        )),
                    }
                }
                ExprKind::StructLiteral { name, fields } => {
                    let Some(struct_type) = self.structs.get(name).cloned() else {
                        return Err(KuError::runtime(
                            format!("undefined struct '{name}'"),
                            expr.span,
                        ));
                    };
                    let mut seen = HashSet::new();
                    for (field_name, value) in fields {
                        if !seen.insert(field_name) {
                            return Err(KuError::runtime(
                                format!("duplicate field '{field_name}' in struct literal"),
                                value.span,
                            ));
                        }
                        let Some(expected) = struct_type.fields.get(field_name) else {
                            return Err(KuError::runtime(
                                format!("struct '{name}' has no field '{field_name}'"),
                                value.span,
                            ));
                        };
                        let actual = self.consume_expr_expecting(value, Some(expected))?;
                        if !type_matches(expected, &actual) {
                            return Err(type_error(value.span, expected, &actual));
                        }
                    }
                    for field_name in struct_type.fields.keys() {
                        if !seen.contains(field_name) {
                            return Err(KuError::runtime(
                                format!("missing field '{field_name}' in struct literal '{name}'"),
                                expr.span,
                            ));
                        }
                    }
                    Ok(Type::Struct(name.clone()))
                }
                ExprKind::ObjectLiteral { fields } => {
                    let mut seen = HashSet::new();
                    let mut object_fields = HashMap::new();
                    for (field_name, value) in fields {
                        if !seen.insert(field_name) {
                            return Err(KuError::runtime(
                                format!("duplicate field '{field_name}' in object literal"),
                                value.span,
                            ));
                        }
                        object_fields.insert(field_name.clone(), self.consume_expr(value)?);
                    }
                    Ok(Type::Object(object_fields))
                }
                ExprKind::Match { value, arms } => self.check_match_expr(value, arms, expr.span),
                ExprKind::Await(task) => {
                    if self.async_depth == 0 {
                        return Err(KuError::runtime(
                            "await can only be used inside async fn",
                            expr.span,
                        ));
                    }
                    match self.consume_await_task_expr(task, expr.span)? {
                        Type::Task(value) => Ok(*value),
                        Type::Unknown => Ok(Type::Unknown),
                        other => Err(KuError::runtime(
                            format!("await expects task but got {}", type_name(&other)),
                            expr.span,
                        )),
                    }
                }
                ExprKind::TryUnwrap { expr: inner } => {
                    if let ExprKind::Index { target, index } = &inner.kind {
                        let target_type = self.check_expr(target)?;
                        if matches!(
                            target_type,
                            Type::Object(_) | Type::StringMap | Type::DynamicObject | Type::KuValue
                        ) {
                            let index_type = self.check_expr(index)?;
                            // A KuValue index accepts a str key (object member) or
                            // an int key (array element); concrete objects/maps
                            // require str keys.
                            let key_ok = if target_type == Type::KuValue {
                                index_type == Type::String || index_type == Type::Int
                            } else {
                                index_type == Type::String
                            };
                            if !key_ok {
                                return Err(type_error(index.span, &Type::String, &index_type));
                            }
                            // `obj[key]?` yields a KuValue — a first-class tagged
                            // dynamic value, not the concrete field type. A
                            // StringMap is homogeneous str, so it unwraps to str.
                            let value_type = match target_type {
                                Type::StringMap => Type::String,
                                _ => Type::KuValue,
                            };
                            // `obj[key]?` unwraps a Result(missing_key): it needs a
                            // Result return type or an enclosing try, and yields the
                            // unwrapped value type (not a nullable).
                            if !matches!(self.current_return, Type::Result(_))
                                && self.recoverable_depth == 0
                            {
                                return Err(KuError::runtime(
                                    "'?' requires a Result return type or an enclosing try block",
                                    expr.span,
                                ));
                            }
                            self.capture_try_exit(TryExitKind::Throw);
                            return Ok(value_type);
                        }
                    }
                    match self.consume_expr(inner)? {
                        Type::Result(value) => {
                            if !matches!(self.current_return, Type::Result(_))
                                && self.recoverable_depth == 0
                            {
                                return Err(KuError::runtime(
                                    "'?' requires a Result return type or an enclosing try block",
                                    expr.span,
                                ));
                            }
                            // `inner` has already been consumed, but the statement
                            // containing this `?` has not yet run its store/re-init.
                            // This is the precise error edge for ownership flow.
                            self.capture_try_exit(TryExitKind::Throw);
                            Ok(*value)
                        }
                        other => Err(KuError::runtime(
                            format!("'?' expects Result but got {}", type_name(&other)),
                            expr.span,
                        )),
                    }
                }
                ExprKind::Function {
                    params,
                    return_type,
                    body,
                } => {
                    self.check_closure_literal(params, return_type.as_ref(), body, expr.span, None)
                }
            }
        })()
        .and_then(|ty| {
            self.reject_readonly_http_native_capture_read(expr, &ty)?;
            Ok(ty)
        });
        self.check_depth = self.check_depth.saturating_sub(1);
        result
    }

    /// Check a closure literal against an optional expected function type drawn
    /// from context (a typed binding, a function-typed parameter, a `return`
    /// position, or a struct field). The expected type only ever fills in
    /// unannotated parameters and is validated against any explicit annotation:
    ///
    /// * rule 4 — with no expected function type, every parameter must be
    ///   explicitly annotated, otherwise it is rejected;
    /// * rule 6 — the expected type's parameter count must match exactly;
    /// * rule 7 — an explicit annotation must agree with the expected type.
    ///
    /// Inference never looks at how the body uses a parameter.
    fn check_closure_literal(
        &mut self,
        params: &[FunctionParam],
        return_type: Option<&TypeName>,
        body: &[Stmt],
        span: Span,
        expected: Option<&Type>,
    ) -> KuResult<Type> {
        reject_duplicate_function_value_params(params)?;

        // Only a non-async function type from context supplies expected params.
        let expected_fn = match expected {
            Some(Type::FunctionValue {
                params,
                is_async: false,
                ..
            }) => Some(params.as_slice()),
            _ => None,
        };

        // rule 6: the expected function type's arity must match exactly.
        if let Some(expected_params) = expected_fn {
            if expected_params.len() != params.len() {
                return Err(KuError::runtime(
                    format!(
                        "closure has {} parameter(s) but the expected function type takes {}",
                        params.len(),
                        expected_params.len()
                    ),
                    span,
                ));
            }
        }

        let mut resolved_params = Vec::with_capacity(params.len());
        for (index, param) in params.iter().enumerate() {
            let expected_param = expected_fn
                .and_then(|expected_params| expected_params.get(index))
                .and_then(|param| param.ty.clone());
            let ty = match &param.ty {
                Some(annotation) => {
                    let annotated = self.resolve_type_name(annotation, param.span)?;
                    // rule 7: an explicit annotation must match the expected type.
                    if let Some(expected_param) = &expected_param {
                        if !type_matches(expected_param, &annotated) {
                            return Err(type_error(param.span, expected_param, &annotated));
                        }
                    }
                    Some(annotated)
                }
                None => match expected_param {
                    Some(ty) => Some(ty),
                    // rule 4: no annotation and no expected type from context.
                    None => {
                        return Err(KuError::runtime(
                            "closure parameter needs a type annotation or an expected function type from context",
                            param.span,
                        ));
                    }
                },
            };
            resolved_params.push(FunctionValueParam {
                name: param.name.clone(),
                ty,
            });
        }

        let own_return = return_type
            .map(|ty| self.resolve_type_name(ty, span).map(Box::new))
            .transpose()?;

        let arg_types = resolved_params
            .iter()
            .map(|param| param.ty.clone().unwrap_or(Type::Unknown))
            .collect::<Vec<_>>();

        let saved_async_depth = self.async_depth;
        self.async_depth = 0;
        let body_id = self.fresh_function_body_id();
        let visible_names = self
            .visible_binding_ids()
            .into_keys()
            .collect::<HashSet<_>>();
        let captured_names = checker_closure_capture_names(params, body, &visible_names);
        self.record_function_body_outer_bindings(body_id, &captured_names);
        let inferred = self.check_function_value_body(
            &resolved_params,
            own_return.as_deref(),
            body,
            Some(body_id),
            &arg_types,
            span,
        );
        self.async_depth = saved_async_depth;
        let inferred = inferred?;

        // The resulting function value's return type: an explicit annotation
        // wins; otherwise always fold in the body-inferred return type. Inferring
        // the *return* type from the body is legitimate and always done (rule 5
        // only forbids reverse-inferring *parameter* types from the body). This
        // lets `f = () => { return 5 }` resolve to `fn(): int`, which is what an
        // owned function value needs to be stored, cloned, or compared.
        let result_return = match own_return {
            Some(return_type) => Some(return_type),
            None => Some(Box::new(inferred)),
        };

        Ok(Type::FunctionValue {
            params: resolved_params,
            return_type: result_return,
            body: body.to_vec(),
            body_id: Some(body_id),
            is_async: false,
        })
    }

    /// Like [`check_expr`], but when `expr` is a closure literal the expected
    /// function type from context is threaded into the closure check.
    fn check_expr_expecting(&mut self, expr: &Expr, expected: Option<&Type>) -> KuResult<Type> {
        if let ExprKind::Function {
            params,
            return_type,
            body,
        } = &expr.kind
        {
            return self.check_closure_literal(
                params,
                return_type.as_ref(),
                body,
                expr.span,
                expected,
            );
        }
        self.check_expr(expr)
    }

    /// Like [`consume_expr`], but threads an expected function type into a
    /// closure literal. A closure literal is a fresh value, so the ownership
    /// move-tracking done by `consume_expr` never applies to it.
    fn consume_expr_expecting(&mut self, expr: &Expr, expected: Option<&Type>) -> KuResult<Type> {
        if matches!(expr.kind, ExprKind::Function { .. }) {
            return self.check_expr_expecting(expr, expected);
        }
        self.consume_expr(expr)
    }

    /// Consume a call argument, but *borrow* function values instead of moving
    /// them (Stage 6d: "call/pass borrows, store moves"). Handing a closure to a
    /// higher-order function or invoking it must not consume the caller's binding
    /// — only an explicit store (binding/field/array element/return) does. Every
    /// other owned type keeps the normal move-on-pass behaviour.
    fn consume_arg_expr_expecting(
        &mut self,
        expr: &Expr,
        expected: Option<&Type>,
    ) -> KuResult<Type> {
        if let ExprKind::Variable(name) = &expr.kind {
            if self.contains(name) {
                let bound = self.get_allow_moved(name, expr.span)?.ty;
                if matches!(bound, Type::FunctionValue { .. }) {
                    // Borrow: verify the binding is still live (not moved-from)
                    // but leave it usable for later calls/passes.
                    return self.check_expr_expecting(expr, expected);
                }
            }
        }
        self.consume_expr_expecting(expr, expected)
    }

    fn check_binary(&self, op: BinaryOp, left: &Type, right: &Type, span: Span) -> KuResult<Type> {
        match op {
            BinaryOp::Add if self.template_mode && can_template_concat(left, right) => {
                Ok(Type::String)
            }
            BinaryOp::Add if left == &Type::String && right == &Type::String => Ok(Type::String),
            BinaryOp::Equal | BinaryOp::NotEqual if type_matches(left, right) => Ok(Type::Bool),
            // A KuValue is a first-class tagged value: `==` / `!=` compare it
            // against anything (by tag + value); arithmetic stays rejected.
            BinaryOp::Equal | BinaryOp::NotEqual
                if left == &Type::KuValue || right == &Type::KuValue =>
            {
                Ok(Type::Bool)
            }
            _ if left == &Type::Unknown || right == &Type::Unknown => Ok(Type::Unknown),
            BinaryOp::Add
            | BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Remainder => numeric_result(op, left, right, span),
            BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
                if is_numeric(left) && is_numeric(right) =>
            {
                Ok(Type::Bool)
            }
            BinaryOp::And | BinaryOp::Or if left == &Type::Bool && right == &Type::Bool => {
                Ok(Type::Bool)
            }
            _ => Err(KuError::runtime(
                format!(
                    "type error: cannot apply operator to {} and {}",
                    type_name(left),
                    type_name(right)
                ),
                span,
            )),
        }
    }

    fn check_template_string(&mut self, raw: &str, span: Span) -> KuResult<()> {
        for interpolation in template_interpolations(raw, span)? {
            let tokens = crate::lexer::Lexer::new(&interpolation.source)
                .tokenize()
                .map_err(|err| map_template_error(err, &interpolation))?;
            let expr = crate::parser::Parser::new(tokens)
                .parse_expression_only()
                .map_err(|err| map_template_error(err, &interpolation))?;
            let saved = self.template_mode;
            self.template_mode = true;
            let result = self.check_expr(&expr);
            self.template_mode = saved;
            result.map_err(|err| map_template_error(err, &interpolation))?;
        }
        Ok(())
    }

    fn check_builtin_call(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Option<Type>> {
        let Some(signature) = metadata::builtin_signature(name) else {
            return Ok(None);
        };
        Ok(Some(self.apply_stdlib_signature(&signature, args, span)?))
    }

    fn check_assign_target(&mut self, target: &AssignTarget, span: Span) -> KuResult<Type> {
        match target {
            AssignTarget::Variable(name) => {
                let binding = self.get(name, span)?;
                if !binding.mutable {
                    return Err(KuError::runtime(
                        format!("cannot assign to immutable variable '{name}'"),
                        span,
                    ));
                }
                Ok(binding.ty)
            }
            AssignTarget::Index { target, index } => {
                let target_type = self.check_expr(target)?;
                let index_type = self.check_expr(index)?;
                match target_type {
                    Type::Array(element) => {
                        if index_type != Type::Int {
                            return Err(type_error(index.span, &Type::Int, &index_type));
                        }
                        Ok(*element)
                    }
                    Type::Unknown => Ok(Type::Unknown),
                    Type::Object(_) => {
                        if index_type != Type::String {
                            return Err(type_error(index.span, &Type::String, &index_type));
                        }
                        Ok(Type::Unknown)
                    }
                    Type::StringMap => {
                        if index_type != Type::String {
                            return Err(type_error(index.span, &Type::String, &index_type));
                        }
                        Ok(Type::String)
                    }
                    Type::DynamicObject => {
                        if index_type != Type::String {
                            return Err(type_error(index.span, &Type::String, &index_type));
                        }
                        Ok(Type::Unknown)
                    }
                    other => Err(KuError::runtime(
                        format!("type error: cannot index {}", type_name(&other)),
                        target.span,
                    )),
                }
            }
            AssignTarget::Field { target, name } => match self.check_expr(target)? {
                Type::Struct(struct_name) => {
                    let Some(struct_type) = self.structs.get(&struct_name) else {
                        return Err(KuError::runtime(
                            format!("undefined struct '{struct_name}'"),
                            target.span,
                        ));
                    };
                    struct_type.fields.get(name).cloned().ok_or_else(|| {
                        KuError::runtime(
                            format!("struct '{struct_name}' has no field '{name}'"),
                            span,
                        )
                    })
                }
                Type::Object(fields) => fields
                    .get(name)
                    .cloned()
                    .ok_or_else(|| KuError::runtime(format!("object has no field '{name}'"), span)),
                Type::StringMap => Ok(Type::String),
                Type::DynamicObject => Ok(Type::Unknown),
                other => Err(KuError::runtime(
                    format!("type error: {} has no fields", type_name(&other)),
                    target.span,
                )),
            },
        }
    }

    fn check_http_service_method_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Option<Type>> {
        let ExprKind::Field { target, name } = &callee.kind else {
            return Ok(None);
        };
        if !matches!(
            name.as_str(),
            "get" | "post" | "put" | "del" | "listen" | "bind" | "run" | "close"
        ) {
            return Ok(None);
        }
        let target_type = self.check_expr(target)?;
        if (name == "run" || name == "close") && is_http_listener_type(&target_type) {
            self.reject_http_handler_control_call(target, &target_type, name, span)?;
            if !args.is_empty() {
                return Err(KuError::runtime(
                    format!(
                        "http listener {name} expects 0 arguments but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            return Ok(Some(Type::Result(Box::new(Type::Null))));
        }
        if !is_http_service_type(&target_type) {
            return Ok(None);
        }
        if name == "run" || name == "close" {
            return Ok(None);
        }
        self.reject_http_handler_control_call(target, &target_type, name, span)?;
        if name == "listen" || name == "bind" {
            if args.len() != 1 {
                return Err(KuError::runtime(
                    format!(
                        "http service {name} expects 1 argument but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let address = self.check_expr(&args[0])?;
            if !type_matches(&Type::String, &address) {
                return Err(type_error(args[0].span, &Type::String, &address));
            }
            if name == "listen" {
                // Native listen owns and frees the server on every return path.
                // Model that ownership transfer so a caught listen error cannot
                // reuse a dangling server pointer.
                self.consume_expr(target)?;
            }
            return Ok(Some(if name == "bind" {
                Type::Result(Box::new(http_listener_type()))
            } else {
                Type::Result(Box::new(Type::Null))
            }));
        }
        if args.len() != 2 {
            return Err(KuError::runtime(
                format!(
                    "http service {name} expects 2 arguments but got {}",
                    args.len()
                ),
                span,
            ));
        }
        let path_type = self.check_expr(&args[0])?;
        if !type_matches(&Type::String, &path_type) {
            return Err(type_error(args[0].span, &Type::String, &path_type));
        }
        let handler_arg = &args[1];
        if let ExprKind::Function {
            params,
            return_type,
            body,
        } = &handler_arg.kind
        {
            reject_duplicate_function_value_params(params)?;
            let params = params
                .iter()
                .map(|param| {
                    Ok(FunctionValueParam {
                        name: param.name.clone(),
                        ty: param
                            .ty
                            .as_ref()
                            .map(|ty| self.resolve_type_name(ty, param.span))
                            .transpose()?,
                    })
                })
                .collect::<KuResult<Vec<_>>>()?;
            let return_type = return_type
                .as_ref()
                .map(|ty| self.resolve_type_name(ty, handler_arg.span).map(Box::new))
                .transpose()?;
            let body_id = self.fresh_function_body_id();
            self.check_http_handler(
                name,
                &params,
                return_type.as_deref(),
                body,
                Some(body_id),
                handler_arg.span,
            )?;
        } else {
            let handler_type = self.check_expr(handler_arg)?;
            match handler_type {
                Type::FunctionValue {
                    params,
                    return_type,
                    body,
                    body_id,
                    ..
                } => {
                    self.check_http_handler(
                        name,
                        &params,
                        return_type.as_deref(),
                        &body,
                        body_id,
                        handler_arg.span,
                    )?;
                }
                Type::Unknown => {
                    return Err(KuError::runtime(
                        "http handler cannot prove a function value is read-only because its type or body is unavailable",
                        handler_arg.span,
                    ));
                }
                _ => {
                    return Err(KuError::runtime(
                        format!("http service {name} handler must be a function"),
                        handler_arg.span,
                    ));
                }
            }
        }
        // Route registration mutates the named service in place. Returning the
        // service would create an untracked second owner of native's raw pointer.
        Ok(Some(Type::Null))
    }

    fn check_http_config_constructor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Option<Type>> {
        let ExprKind::Field { target, name } = &callee.kind else {
            return Ok(None);
        };
        if !matches!(name.as_str(), "client" | "service" | "server") {
            return Ok(None);
        }
        let ExprKind::Variable(module) = &target.kind else {
            return Ok(None);
        };
        if module != "http" || self.contains("http") || !self.std_modules.contains("http") {
            return Ok(None);
        }
        if args.len() > 1 {
            return Err(KuError::runtime(
                format!(
                    "http.{name} expects 0 or 1 arguments but got {}",
                    args.len()
                ),
                span,
            ));
        }
        if let Some(config) = args.first() {
            let config_type = self.check_expr(config)?;
            if !matches!(
                config_type,
                Type::Object(_) | Type::DynamicObject | Type::Unknown
            ) {
                return Err(type_error(
                    config.span,
                    &Type::Object(HashMap::new()),
                    &config_type,
                ));
            }
            let allowed = if name == "client" {
                &HTTP_CLIENT_CONFIG_FIELDS[..]
            } else {
                &HTTP_SERVICE_CONFIG_FIELDS[..]
            };
            validate_http_config_fields(&config_type, allowed, config.span)?;
        }
        Ok(Some(if name == "client" {
            http_client_type()
        } else {
            http_service_type()
        }))
    }

    fn apply_http_config_constructor_signature(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Type> {
        if args.len() > 1 {
            return Err(KuError::runtime(
                format!(
                    "function '{name}' expects 0 or 1 arguments but got {}",
                    args.len()
                ),
                span,
            ));
        }
        if let Some(config) = args.first() {
            let config_type = self.check_expr(config)?;
            if !matches!(
                config_type,
                Type::Object(_) | Type::DynamicObject | Type::Unknown
            ) {
                return Err(type_error(
                    config.span,
                    &Type::Object(HashMap::new()),
                    &config_type,
                ));
            }
            let allowed = if name == "http.client" {
                &HTTP_CLIENT_CONFIG_FIELDS[..]
            } else {
                &HTTP_SERVICE_CONFIG_FIELDS[..]
            };
            validate_http_config_fields(&config_type, allowed, config.span)?;
        }
        Ok(if name == "http.client" {
            http_client_type()
        } else {
            http_service_type()
        })
    }

    fn apply_redis_client_constructor_signature(
        &mut self,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Type> {
        expect_arg_count("redis.client", args.len(), 1, span)?;
        let config = &args[0];
        let config_type = self.check_expr(config)?;
        if !matches!(
            config_type,
            Type::Object(_) | Type::DynamicObject | Type::Unknown
        ) {
            return Err(type_error(
                config.span,
                &Type::Object(HashMap::new()),
                &config_type,
            ));
        }
        validate_redis_client_config(&config_type, config.span)?;
        Ok(Type::Result(Box::new(Type::Native(
            metadata::REDIS_CLIENT.to_string(),
        ))))
    }

    fn apply_net_client_constructor_signature(
        &mut self,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Type> {
        expect_arg_count("net.client", args.len(), 1, span)?;
        let config = &args[0];
        let config_type = self.check_expr(config)?;
        if !matches!(
            config_type,
            Type::Object(_) | Type::DynamicObject | Type::Unknown
        ) {
            return Err(type_error(
                config.span,
                &Type::Object(HashMap::new()),
                &config_type,
            ));
        }
        validate_net_client_config(&config_type, config.span)?;
        validate_net_tls_literal(config)?;
        Ok(Type::Result(Box::new(Type::Native(
            metadata::NET_CLIENT.to_string(),
        ))))
    }

    fn apply_mysql_client_constructor_signature(
        &mut self,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Type> {
        expect_arg_count("mysql.client", args.len(), 1, span)?;
        let config = &args[0];
        let config_type = self.check_expr(config)?;
        if !matches!(
            config_type,
            Type::Object(_) | Type::DynamicObject | Type::Unknown
        ) {
            return Err(type_error(
                config.span,
                &Type::Object(HashMap::new()),
                &config_type,
            ));
        }
        validate_mysql_client_config(&config_type, config.span)?;
        Ok(Type::Result(Box::new(Type::Native(
            metadata::MYSQL_CLIENT.to_string(),
        ))))
    }

    fn apply_pg_client_signature(&mut self, args: &[Expr], span: Span) -> KuResult<Type> {
        expect_arg_count("pg.client", args.len(), 1, span)?;
        let config_type = self.check_expr(&args[0])?;
        match &config_type {
            Type::Object(fields) => {
                const ALLOWED: [&str; 6] = [
                    "conninfo",
                    "max_connections",
                    "max_waiters",
                    "connect_timeout_ms",
                    "acquire_timeout_ms",
                    "query_timeout_ms",
                ];
                let mut unknown = fields
                    .keys()
                    .filter(|key| !ALLOWED.contains(&key.as_str()))
                    .collect::<Vec<_>>();
                unknown.sort();
                if let Some(key) = unknown.first() {
                    return Err(KuError::runtime(
                        format!("unknown pg client config field '{key}'"),
                        args[0].span,
                    ));
                }
                let Some(conninfo) = fields.get("conninfo") else {
                    return Err(KuError::runtime(
                        "pg.client config requires string field 'conninfo'",
                        args[0].span,
                    ));
                };
                if !type_matches(&Type::String, conninfo) {
                    return Err(KuError::runtime(
                        "pg.client config field 'conninfo' must be str",
                        args[0].span,
                    ));
                }
                for field in ALLOWED.iter().skip(1) {
                    if let Some(actual) = fields.get(*field) {
                        if !type_matches(&Type::Int, actual) {
                            return Err(KuError::runtime(
                                format!("pg.client config field '{field}' must be int"),
                                args[0].span,
                            ));
                        }
                    }
                }
            }
            Type::DynamicObject | Type::Unknown => {}
            actual => {
                return Err(type_error(
                    args[0].span,
                    &Type::Object(HashMap::new()),
                    actual,
                ));
            }
        }
        Ok(Type::Result(Box::new(Type::Native(
            metadata::PG_CLIENT.to_string(),
        ))))
    }

    fn apply_pg_client_query_signature(&mut self, args: &[Expr], span: Span) -> KuResult<Type> {
        if args.len() != 3 {
            return Err(KuError::runtime(
                "PostgreSQL client query requires query(sql, params); pass [] when there are no parameters",
                span,
            ));
        }
        let client = self.check_expr(&args[0])?;
        let expected_client = Type::Native(metadata::PG_CLIENT.to_string());
        if client != expected_client {
            return Err(type_error(args[0].span, &expected_client, &client));
        }
        let sql = self.check_expr(&args[1])?;
        if sql != Type::String {
            return Err(type_error(args[1].span, &Type::String, &sql));
        }
        let params = self.check_expr(&args[2])?;
        let empty_literal = matches!(&args[2].kind, ExprKind::Array(values) if values.is_empty());
        let expected_params = Type::Array(Box::new(Type::String));
        if params != expected_params && !empty_literal {
            return Err(type_error(args[2].span, &expected_params, &params));
        }
        self.reject_effectful_args_on_captured_native_receiver(
            &args[0],
            &client,
            &args[1..],
            span,
        )?;
        Ok(Type::Result(Box::new(Type::Native(
            metadata::PG_RESULT.to_string(),
        ))))
    }

    fn check_http_handler(
        &mut self,
        method: &str,
        params: &[FunctionValueParam],
        return_type: Option<&Type>,
        body: &[Stmt],
        body_id: Option<FunctionBodyId>,
        span: Span,
    ) -> KuResult<()> {
        if params.len() > 1 {
            return Err(KuError::runtime(
                format!(
                    "ordinary HTTP route handler for {method} accepts fn() or fn(req); fn(req, res) is not allowed"
                ),
                span,
            ));
        }
        if body.is_empty() || body_id.is_none() {
            return Err(KuError::runtime(
                "http handler cannot prove a function value is read-only because its body is unavailable",
                span,
            ));
        }
        let arg_types = if let Some(param) = params.first() {
            if matches!(param.name.as_str(), "res" | "writer") {
                return Err(KuError::runtime(
                    "http handler parameter must be named req, or _req when an adapter requires an unused request parameter; res/writer parameters are not allowed in ordinary handlers",
                    span,
                ));
            }
            if param.name != "req" && param.name != "_req" {
                return Err(KuError::runtime(
                    format!(
                        "http handler parameter must be named req, or _req when an adapter requires an unused request parameter; got '{}'",
                        param.name
                    ),
                    span,
                ));
            }
            if param.name == "req" && !function_body_uses_name(body, "req") {
                return Err(KuError::runtime(
                    "http handler parameter 'req' is unused; write fn() when the route does not need the request",
                    span,
                ));
            }
            vec![http_request_type()]
        } else {
            Vec::new()
        };
        reject_http_side_effect_response_calls(body, span)?;
        let response = http_response_type();
        let result_response = Type::Result(Box::new(response.clone()));
        let allowed = union_or_single(vec![response.clone(), result_response.clone()]);
        if let Some(return_type) = return_type {
            if !type_matches(&allowed, return_type) {
                return Err(http_handler_return_error(span, return_type));
            }
        }
        if !block_may_return(body) {
            return Err(http_handler_return_error(span, &Type::Null));
        }
        let actual = self.check_function_value_call_with_types_readonly_captures(
            FunctionValueBodyRef {
                params,
                return_type,
                body,
                body_id,
            },
            &arg_types,
            span,
        )?;
        if !type_matches(&allowed, &actual) {
            return Err(http_handler_return_error(span, &actual));
        }
        Ok(())
    }

    fn check_enum_constructor(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Type> {
        let Some(enum_type) = self.enums.get(enum_name) else {
            return Err(KuError::runtime(
                format!("undefined enum '{enum_name}'"),
                span,
            ));
        };
        let Some(expected_fields) = enum_type.variants.get(variant).cloned() else {
            return Err(KuError::runtime(
                format!("enum '{enum_name}' has no variant '{variant}'"),
                span,
            ));
        };
        if expected_fields.len() != args.len() {
            return Err(KuError::runtime(
                format!(
                    "enum variant '{enum_name}.{variant}' expects {} arguments but got {}",
                    expected_fields.len(),
                    args.len()
                ),
                span,
            ));
        }
        for (arg, expected) in args.iter().zip(expected_fields.iter()) {
            let actual = self.consume_expr(arg)?;
            if !type_matches(expected, &actual) {
                return Err(type_error(arg.span, expected, &actual));
            }
        }
        Ok(Type::Enum(enum_name.to_string()))
    }

    fn check_match_expr(&mut self, value: &Expr, arms: &[MatchArm], span: Span) -> KuResult<Type> {
        if arms.is_empty() {
            return Err(KuError::runtime("match requires at least one arm", span));
        }
        let value_type = self.check_expr(value)?;
        // Binding an owned enum payload MOVES it out of the scrutinee (the backend
        // clears the slot), so a match that binds one consumes the scrutinee.
        // Record that move after the arms are checked, so later uses (a second
        // match, a clone, passing it on) are rejected instead of silently reading
        // an emptied payload.
        let consumes_scrutinee = self.match_consumes_scrutinee(&value_type, arms);
        let before_arms = self.scopes.clone();
        let mut arm_scopes = Vec::with_capacity(arms.len());
        let mut result_type = Type::Unknown;
        let mut saw_unguarded_catch_all = false;
        let mut covered_full_variants = HashSet::new();
        let mut covered_patterns = HashSet::new();
        for arm in arms {
            self.scopes = before_arms.clone();
            if saw_unguarded_catch_all {
                return Err(KuError::runtime(
                    "match arm after catch-all pattern is unreachable",
                    arm.span,
                ));
            }
            self.push_scope();
            if let Err(err) = self.check_match_pattern(&arm.pattern, &value_type, arm.span) {
                self.pop_scope();
                return Err(err);
            }
            if let MatchPattern::EnumVariant {
                enum_name, variant, ..
            } = &arm.pattern
            {
                if covered_full_variants.contains(variant) {
                    self.pop_scope();
                    return Err(KuError::runtime(
                        format!("match arm for '{enum_name}.{variant}' is unreachable"),
                        arm.span,
                    ));
                }
            }
            if arm.guard.is_none() {
                if pattern_is_catch_all(&arm.pattern) {
                    saw_unguarded_catch_all = true;
                }
                if let MatchPattern::EnumVariant { variant, .. } = &arm.pattern {
                    if enum_pattern_covers_all_payload(&arm.pattern) {
                        covered_full_variants.insert(variant.clone());
                    }
                }
                let key = pattern_key(&arm.pattern);
                if covered_patterns.contains(&key) {
                    self.pop_scope();
                    return Err(KuError::runtime(
                        "match arm pattern is unreachable",
                        arm.span,
                    ));
                }
                covered_patterns.insert(key);
            }
            if let Some(guard) = &arm.guard {
                let guard_type = self.check_expr(guard)?;
                if guard_type != Type::Bool {
                    self.pop_scope();
                    return Err(type_error(guard.span, &Type::Bool, &guard_type));
                }
            }
            let actual = self.consume_expr(&arm.value);
            self.pop_scope();
            let actual = actual?;
            arm_scopes.push(self.scopes.clone());
            if result_type == Type::Unknown {
                result_type = actual;
            } else if !type_matches(&result_type, &actual) {
                return Err(type_error(arm.value.span, &result_type, &actual));
            }
        }
        self.scopes = merge_moved_scope_paths(before_arms, &arm_scopes);
        if !saw_unguarded_catch_all {
            if let Type::Enum(enum_name) = &value_type {
                let Some(enum_type) = self.enums.get(enum_name) else {
                    return Err(KuError::runtime(
                        format!("undefined enum '{enum_name}'"),
                        span,
                    ));
                };
                let missing = enum_type
                    .variants
                    .keys()
                    .filter(|variant| !covered_full_variants.contains(*variant))
                    .cloned()
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    return Err(KuError::runtime(
                        format!(
                            "match on enum '{enum_name}' is not exhaustive; missing {}",
                            missing.join(", ")
                        ),
                        span,
                    ));
                }
            }
        }
        if consumes_scrutinee {
            self.consume_expr(value)?;
        }
        Ok(result_type)
    }

    /// True when matching `value` binds an owned enum payload — the backend moves
    /// that payload out of the scrutinee, so the match consumes it.
    fn match_consumes_scrutinee(&self, value_type: &Type, arms: &[MatchArm]) -> bool {
        let Type::Enum(enum_name) = value_type else {
            return false;
        };
        let Some(enum_type) = self.enums.get(enum_name) else {
            return false;
        };
        arms.iter().any(|arm| {
            let MatchPattern::EnumVariant {
                variant, fields, ..
            } = &arm.pattern
            else {
                return false;
            };
            let Some(field_types) = enum_type.variants.get(variant) else {
                return false;
            };
            fields
                .iter()
                .zip(field_types)
                .any(|(pat, ty)| matches!(pat, MatchPattern::Binding(_)) && self.is_owned_type(ty))
        })
    }

    fn check_match_pattern(
        &mut self,
        pattern: &MatchPattern,
        expected: &Type,
        span: Span,
    ) -> KuResult<()> {
        match pattern {
            MatchPattern::Wildcard => Ok(()),
            MatchPattern::Binding(name) => self.define(name.clone(), expected.clone(), false, span),
            MatchPattern::Literal(literal) => {
                let actual = type_of_literal(literal);
                if type_matches(expected, &actual) {
                    Ok(())
                } else {
                    Err(type_error(span, expected, &actual))
                }
            }
            MatchPattern::EnumVariant {
                enum_name,
                variant,
                fields,
            } => {
                let expected_enum = Type::Enum(enum_name.clone());
                if !type_matches(expected, &expected_enum) {
                    return Err(type_error(span, expected, &expected_enum));
                }
                let Some(enum_type) = self.enums.get(enum_name) else {
                    return Err(KuError::runtime(
                        format!("undefined enum '{enum_name}'"),
                        span,
                    ));
                };
                let Some(payload) = enum_type.variants.get(variant).cloned() else {
                    return Err(KuError::runtime(
                        format!("enum '{enum_name}' has no variant '{variant}'"),
                        span,
                    ));
                };
                if payload.len() != fields.len() {
                    return Err(KuError::runtime(
                        format!(
                            "match pattern '{enum_name}.{variant}' expects {} fields but got {}",
                            payload.len(),
                            fields.len()
                        ),
                        span,
                    ));
                }
                for (field, ty) in fields.iter().zip(payload.iter()) {
                    self.check_match_pattern(field, ty, span)?;
                }
                Ok(())
            }
        }
    }

    fn check_dotted_builtin_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Option<Type>> {
        let Some((module, function)) = dotted_name(callee) else {
            return Ok(None);
        };
        if self.contains(&module) {
            return Ok(None);
        }
        if module == "time" {
            return Ok(Some(self.check_time_call(&function, args, span)?));
        }
        if module == "pg" && function != "client" {
            return Err(KuError::runtime(
                format!(
                    "PostgreSQL module API 'pg.{function}' is not public; construct one pooled client with 'pg.client(config)?' and call receiver methods"
                ),
                span,
            ));
        }
        if matches!(module.as_str(), "pg_client" | "pg_result") {
            return Err(KuError::runtime(
                format!(
                    "PostgreSQL compiler-internal API '{module}.{function}' is not public; call the receiver method instead"
                ),
                span,
            ));
        }
        let Some(signature) = metadata::dotted_signature(&module, &function) else {
            if metadata::is_std_module(&module) && self.std_modules.contains(&module) {
                return Err(KuError::runtime(
                    format!("unknown stdlib function '{module}.{function}'"),
                    span,
                ));
            }
            return Ok(None);
        };
        if metadata::module_requires_import(&module) && !self.std_modules.contains(&module) {
            return Err(KuError::runtime(
                format!("std module '{module}' must be imported before use"),
                span,
            ));
        }
        Ok(Some(self.apply_stdlib_signature(&signature, args, span)?))
    }

    fn check_time_call(&mut self, function: &str, args: &[Expr], span: Span) -> KuResult<Type> {
        let actuals = args
            .iter()
            .map(|arg| self.check_expr(arg))
            .collect::<KuResult<Vec<_>>>()?;
        match function {
            "steady_millis" => {
                expect_arg_count("time.steady_millis", args.len(), 0, span)?;
                Ok(Type::Int)
            }
            "now" => {
                expect_arg_count("time.now", args.len(), 0, span)?;
                Ok(Type::Int)
            }
            "instant" => {
                expect_arg_count("time.instant", args.len(), 0, span)?;
                Ok(Type::DynamicObject)
            }
            "elapsed" => {
                expect_arg_count("time.elapsed", args.len(), 1, span)?;
                expect_dynamic_object_arg("time.elapsed", &actuals[0], args[0].span)?;
                Ok(Type::Int)
            }
            "unix" => {
                expect_arg_count_range("time.unix", args.len(), 0, 1, span)?;
                if let Some(actual) = actuals.first() {
                    expect_dynamic_object_arg("time.unix", actual, args[0].span)?;
                }
                Ok(Type::Int)
            }
            "millis" => {
                expect_arg_count_range("time.millis", args.len(), 0, 1, span)?;
                if let Some(actual) = actuals.first() {
                    expect_dynamic_object_arg("time.millis", actual, args[0].span)?;
                }
                Ok(Type::Int)
            }
            "from_unix" | "from_millis" => {
                expect_arg_count(&format!("time.{function}"), args.len(), 1, span)?;
                expect_type_arg(&actuals[0], &Type::Int, args[0].span)?;
                Ok(Type::DynamicObject)
            }
            "date" => match args.len() {
                0 => Ok(Type::DynamicObject),
                1 => {
                    expect_dynamic_object_arg("time.date", &actuals[0], args[0].span)?;
                    Ok(Type::DynamicObject)
                }
                2 => {
                    expect_dynamic_object_arg("time.date", &actuals[0], args[0].span)?;
                    expect_type_arg(&actuals[1], &Type::String, args[1].span)?;
                    Ok(Type::Result(Box::new(Type::DynamicObject)))
                }
                3 => {
                    for (actual, arg) in actuals.iter().zip(args) {
                        expect_type_arg(actual, &Type::Int, arg.span)?;
                    }
                    Ok(Type::Result(Box::new(Type::DynamicObject)))
                }
                _ => Err(KuError::runtime(
                    format!(
                        "function 'time.date' expects 0, 1, 2, or 3 arguments but got {}",
                        args.len()
                    ),
                    span,
                )),
            },
            "datetime" => {
                if args.len() != 6 && args.len() != 7 {
                    return Err(KuError::runtime(
                        format!(
                            "function 'time.datetime' expects 6 or 7 arguments but got {}",
                            args.len()
                        ),
                        span,
                    ));
                }
                for (index, actual) in actuals.iter().enumerate() {
                    let expected = if index == 6 { Type::String } else { Type::Int };
                    expect_type_arg(actual, &expected, args[index].span)?;
                }
                Ok(Type::Result(Box::new(Type::DynamicObject)))
            }
            "format" => match args.len() {
                1 => {
                    expect_dynamic_object_arg("time.format", &actuals[0], args[0].span)?;
                    Ok(Type::String)
                }
                2 | 3 => {
                    expect_dynamic_object_arg("time.format", &actuals[0], args[0].span)?;
                    expect_type_arg(&actuals[1], &Type::String, args[1].span)?;
                    if args.len() == 3 {
                        expect_type_arg(&actuals[2], &Type::String, args[2].span)?;
                    }
                    Ok(Type::Result(Box::new(Type::String)))
                }
                _ => Err(KuError::runtime(
                    format!(
                        "function 'time.format' expects 1, 2, or 3 arguments but got {}",
                        args.len()
                    ),
                    span,
                )),
            },
            "parse" => {
                if args.is_empty() || args.len() > 3 {
                    return Err(KuError::runtime(
                        format!(
                            "function 'time.parse' expects 1, 2, or 3 arguments but got {}",
                            args.len()
                        ),
                        span,
                    ));
                }
                for (actual, arg) in actuals.iter().zip(args) {
                    expect_type_arg(actual, &Type::String, arg.span)?;
                }
                Ok(Type::Result(Box::new(Type::DynamicObject)))
            }
            "duration" => {
                if args.len() != 1 && args.len() != 2 {
                    return Err(KuError::runtime(
                        format!(
                            "function 'time.duration' expects 1 or 2 arguments but got {}",
                            args.len()
                        ),
                        span,
                    ));
                }
                expect_type_arg(&actuals[0], &Type::Int, args[0].span)?;
                if args.len() == 2 {
                    expect_type_arg(&actuals[1], &Type::String, args[1].span)?;
                }
                Ok(Type::Result(Box::new(Type::DynamicObject)))
            }
            "add" | "sub" | "diff" | "compare" => {
                expect_arg_count(&format!("time.{function}"), args.len(), 2, span)?;
                expect_dynamic_object_arg(&format!("time.{function}"), &actuals[0], args[0].span)?;
                expect_dynamic_object_arg(&format!("time.{function}"), &actuals[1], args[1].span)?;
                Ok(match function {
                    "compare" => Type::Int,
                    "diff" => Type::DynamicObject,
                    _ => Type::DynamicObject,
                })
            }
            "parts" => match args.len() {
                1 => {
                    expect_dynamic_object_arg("time.parts", &actuals[0], args[0].span)?;
                    Ok(Type::DynamicObject)
                }
                2 => {
                    expect_dynamic_object_arg("time.parts", &actuals[0], args[0].span)?;
                    expect_type_arg(&actuals[1], &Type::String, args[1].span)?;
                    Ok(Type::Result(Box::new(Type::DynamicObject)))
                }
                _ => Err(KuError::runtime(
                    format!(
                        "function 'time.parts' expects 1 or 2 arguments but got {}",
                        args.len()
                    ),
                    span,
                )),
            },
            "weekday" => match args.len() {
                1 => {
                    expect_dynamic_object_arg("time.weekday", &actuals[0], args[0].span)?;
                    Ok(Type::Int)
                }
                2 => {
                    expect_dynamic_object_arg("time.weekday", &actuals[0], args[0].span)?;
                    expect_type_arg(&actuals[1], &Type::String, args[1].span)?;
                    Ok(Type::Result(Box::new(Type::Int)))
                }
                _ => Err(KuError::runtime(
                    format!(
                        "function 'time.weekday' expects 1 or 2 arguments but got {}",
                        args.len()
                    ),
                    span,
                )),
            },
            "is_leap" => {
                expect_arg_count("time.is_leap", args.len(), 1, span)?;
                expect_type_arg(&actuals[0], &Type::Int, args[0].span)?;
                Ok(Type::Bool)
            }
            "days_in_month" => {
                expect_arg_count("time.days_in_month", args.len(), 2, span)?;
                expect_type_arg(&actuals[0], &Type::Int, args[0].span)?;
                expect_type_arg(&actuals[1], &Type::Int, args[1].span)?;
                Ok(Type::Result(Box::new(Type::Int)))
            }
            "sleep" => {
                expect_arg_count("time.sleep", args.len(), 1, span)?;
                if actuals[0] != Type::Int
                    && !matches!(actuals[0], Type::DynamicObject | Type::Object(_))
                {
                    return Err(type_error(args[0].span, &Type::Int, &actuals[0]));
                }
                Ok(Type::Result(Box::new(Type::Null)))
            }
            _ => Err(KuError::runtime(
                format!("unknown stdlib function 'time.{function}'"),
                span,
            )),
        }
    }

    fn check_std_method_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Option<Type>> {
        let ExprKind::Field { target, name } = &callee.kind else {
            return Ok(None);
        };
        let target_type = self.check_expr(target)?;
        if name == "clone" {
            expect_arg_count("clone", args.len(), 0, span)?;
            if is_http_service_type(&target_type) {
                return Err(KuError::runtime(
                    "http service values cannot be cloned",
                    span,
                ));
            }
            if contains_task_type(&target_type) {
                return Err(KuError::runtime("task values cannot be cloned", span));
            }
            // `obj["key"]` erases its element type, so resolve the declared one —
            // otherwise the clone the index rule tells the user to add is itself
            // rejected, leaving no legal way to read an owned element.
            let target_type = match target_type {
                Type::Unknown => self
                    .static_index_element_type(target)
                    .unwrap_or(Type::Unknown),
                other => other,
            };
            // Native values own process-external resources (sockets, database
            // connections/results, pools, ...).  They are move-only: the native
            // backends deliberately have no meaningful clone operation.  Letting
            // this reach code generation used to turn otherwise valid Ku source
            // into an unconditional process exit at runtime.
            if self.type_contains_native_resource(&target_type) {
                return Err(KuError::runtime(
                    "native resource handles cannot be cloned",
                    span,
                ));
            }
            if self.is_owned_type(&target_type) {
                // Cloning reads the WHOLE value, so it must be fully live: a
                // partially-moved struct would clone an already-emptied field.
                if let PlaceClass::Movable(place) = self.classify_place(target) {
                    self.check_place_fully_live(&place, span)?;
                }
                return Ok(Some(target_type));
            }
            return Err(KuError::runtime(
                format!(
                    "clone() is only available on owned values, got {}",
                    type_name(&target_type)
                ),
                span,
            ));
        }
        let is_database_resource = matches!(
            &target_type,
            Type::Native(native)
                if native == metadata::PG_CLIENT
                    || native == metadata::PG_RESULT
                    || native == metadata::REDIS_CLIENT
                    || native == metadata::MYSQL_CLIENT
                    || native == metadata::MYSQL_RESULT
        );
        if is_database_resource && !matches!(self.classify_place(target), PlaceClass::Movable(_)) {
            return Err(KuError::runtime(
                "database client/result method receivers must be assigned to a binding before use",
                span,
            ));
        }
        if matches!(&target_type, Type::Native(native) if native == metadata::NET_CLIENT)
            && !matches!(self.classify_place(target), PlaceClass::Movable(_))
        {
            return Err(KuError::runtime(
                "net client method receivers must be assigned to a binding before use",
                span,
            ));
        }
        if matches!(&target_type, Type::Native(native) if native == metadata::BYTES)
            && !matches!(self.classify_place(target), PlaceClass::Movable(_))
        {
            return Err(KuError::runtime(
                "bytes method receivers must be assigned to a binding before use",
                span,
            ));
        }
        self.reject_effectful_args_on_captured_native_receiver(target, &target_type, args, span)?;
        if let Type::Array(element) = &target_type {
            // These APIs return a fresh element/array or pass an element by value,
            // so native lowering must clone the element. Move-only handles have no
            // valid clone operation; reject the source instead of reaching the
            // backend's defensive forbidden-clone trap.
            if matches!(
                name.as_str(),
                "first" | "last" | "try_get" | "push" | "concat"
            ) && self.type_contains_native_resource(element)
            {
                return Err(KuError::runtime(
                    format!("array.{name} cannot clone move-only native resource elements"),
                    span,
                ));
            }
        }
        if matches!(&target_type, Type::Native(native) if native == metadata::REDIS_CLIENT) {
            let Some(signature) = metadata::redis_client_method_signature(name) else {
                return Ok(None);
            };
            let mut method_args = Vec::with_capacity(args.len() + 1);
            method_args.push((**target).clone());
            method_args.extend(args.iter().cloned());
            return self
                .apply_stdlib_signature(&signature, &method_args, span)
                .map(Some);
        }
        if let Type::Native(native) = &target_type {
            if native == metadata::BYTES {
                let Some(signature) = metadata::bytes_method_signature(name) else {
                    return Ok(None);
                };
                let mut method_args = Vec::with_capacity(args.len() + 1);
                method_args.push((**target).clone());
                method_args.extend(args.iter().cloned());
                return self
                    .apply_stdlib_signature(&signature, &method_args, span)
                    .map(Some);
            }
            if native == metadata::NET_CLIENT {
                let Some(signature) = metadata::net_client_method_signature(name) else {
                    return Ok(None);
                };
                let mut method_args = Vec::with_capacity(args.len() + 1);
                method_args.push((**target).clone());
                method_args.extend(args.iter().cloned());
                return self
                    .apply_stdlib_signature(&signature, &method_args, span)
                    .map(Some);
            }
            if native == metadata::MYSQL_CLIENT || native == metadata::MYSQL_RESULT {
                let Some(signature) = metadata::mysql_method_signature(native, name) else {
                    return Ok(None);
                };
                let mut method_args = Vec::with_capacity(args.len() + 1);
                method_args.push((**target).clone());
                method_args.extend(args.iter().cloned());
                return self
                    .apply_stdlib_signature(&signature, &method_args, span)
                    .map(Some);
            }
        }
        if let Type::Task(value) = target_type {
            let _ = value;
            return match name.as_str() {
                "status" => {
                    expect_arg_count("task.status", args.len(), 0, span)?;
                    Err(KuError::runtime(
                        "task handles can only be awaited; status() is not part of Ku's user task API",
                        span,
                    ))
                }
                "cancel" => {
                    expect_arg_count("task.cancel", args.len(), 0, span)?;
                    Err(KuError::runtime(
                        "task handles can only be awaited; cancel() is not part of Ku's user task API",
                        span,
                    ))
                }
                "await_timeout" => {
                    expect_arg_count("task.await_timeout", args.len(), 1, span)?;
                    let timeout = self.check_expr(&args[0])?;
                    if timeout != Type::Int {
                        return Err(type_error(args[0].span, &Type::Int, &timeout));
                    }
                    Err(KuError::runtime(
                        "task handles can only be awaited; await_timeout() is not part of Ku's user task API",
                        span,
                    ))
                }
                _ => Ok(None),
            };
        }
        let module = match &target_type {
            Type::String => "string",
            Type::Array(_) if name != "map" => "array",
            Type::Object(_) | Type::StringMap | Type::DynamicObject if name == "get_or" => "object",
            Type::KuValue if name == "as_int" || name == "as_str" => "kuvalue",
            Type::Native(native)
                if native == metadata::PG_CLIENT && matches!(name.as_str(), "query" | "close") =>
            {
                "pg_client"
            }
            Type::Native(native)
                if native == metadata::PG_RESULT
                    && matches!(name.as_str(), "rows" | "cols" | "value" | "is_null") =>
            {
                "pg_result"
            }
            _ => return Ok(None),
        };
        let Some(signature) = metadata::dotted_signature(module, name) else {
            return Ok(None);
        };
        let mut method_args = Vec::with_capacity(args.len() + 1);
        method_args.push((**target).clone());
        method_args.extend(args.iter().cloned());
        self.apply_stdlib_signature(&signature, &method_args, span)
            .map(Some)
    }

    fn apply_stdlib_signature(
        &mut self,
        signature: &Signature,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Type> {
        if matches!(
            signature.name.as_str(),
            "http.client" | "http.service" | "http.server"
        ) {
            return self.apply_http_config_constructor_signature(&signature.name, args, span);
        }
        if signature.name == "http.request" {
            expect_arg_count("http.request", args.len(), 1, span)?;
            let config_type = self.check_expr(&args[0])?;
            if !matches!(
                config_type,
                Type::Object(_) | Type::StringMap | Type::DynamicObject
            ) {
                return Err(type_error(args[0].span, &Type::DynamicObject, &config_type));
            }
            validate_http_config_fields(&config_type, &HTTP_REQUEST_CONFIG_FIELDS, args[0].span)?;
            return Ok(Type::Result(Box::new(http_response_type())));
        }
        if signature.name == "redis.client" {
            return self.apply_redis_client_constructor_signature(args, span);
        }
        if signature.name == "net.client" {
            return self.apply_net_client_constructor_signature(args, span);
        }
        if signature.name == "mysql.client" {
            return self.apply_mysql_client_constructor_signature(args, span);
        }
        if signature.name == "pg.client" {
            return self.apply_pg_client_signature(args, span);
        }
        if signature.name == "pg_client.query" {
            return self.apply_pg_client_query_signature(args, span);
        }
        if matches!(
            signature.name.as_str(),
            "http.text"
                | "http.html"
                | "http.json"
                | "http.empty"
                | "http.redirect"
                | "http.statusText"
        ) {
            return self.apply_http_response_helper_signature(&signature.name, args, span);
        }
        if signature.name == "object.get_or" {
            return self.apply_object_get_or_signature(args, span);
        }
        expect_arg_count(&signature.name, args.len(), signature.args.len(), span)?;
        let consuming = stdlib_consuming_args(&signature.name);
        let actuals = args
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                if consuming.contains(&index) {
                    self.consume_expr(arg)
                } else {
                    self.check_expr(arg)
                }
            })
            .collect::<KuResult<Vec<_>>>()?;
        if let Some(receiver_type) = actuals.first() {
            self.reject_effectful_args_on_captured_native_receiver(
                &args[0],
                receiver_type,
                &args[1..],
                span,
            )?;
        }
        for (index, rule) in signature.args.iter().enumerate() {
            self.check_stdlib_arg(rule, index, args, &actuals)?;
        }
        Self::stdlib_pattern_to_type(&signature.returns, &actuals, span)
    }

    fn check_stdlib_arg(
        &self,
        rule: &ArgRule,
        index: usize,
        args: &[Expr],
        actuals: &[Type],
    ) -> KuResult<()> {
        match rule {
            ArgRule::Is(pattern) => {
                if Self::type_matches_pattern(&actuals[index], pattern) {
                    Ok(())
                } else {
                    Err(type_error(
                        args[index].span,
                        &Self::pattern_expected_type(pattern),
                        &actuals[index],
                    ))
                }
            }
            ArgRule::MatchesArrayElement { array_arg } => {
                let Type::Array(element) = &actuals[*array_arg] else {
                    return Err(type_error(
                        args[*array_arg].span,
                        &Type::Array(Box::new(Type::Unknown)),
                        &actuals[*array_arg],
                    ));
                };
                if type_matches(element, &actuals[index]) {
                    Ok(())
                } else {
                    Err(type_error(args[index].span, element, &actuals[index]))
                }
            }
            ArgRule::MatchesArrayArg { array_arg } => match (&actuals[*array_arg], &actuals[index])
            {
                (Type::Array(left), Type::Array(right)) if type_matches(left, right) => Ok(()),
                (Type::Array(_), Type::Array(_)) => Err(type_error(
                    args[index].span,
                    &actuals[*array_arg],
                    &actuals[index],
                )),
                _ => Err(type_error(
                    args[index].span,
                    &Type::Array(Box::new(Type::Unknown)),
                    &actuals[index],
                )),
            },
        }
    }

    fn apply_http_response_helper_signature(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Type> {
        match name {
            "http.text" | "http.html" => {
                if args.len() == 1 {
                    let body = self.consume_expr(&args[0])?;
                    if body == Type::String {
                        Ok(http_response_type())
                    } else {
                        Err(type_error(args[0].span, &Type::String, &body))
                    }
                } else if args.len() == 2 {
                    self.check_http_status_arg(&args[0])?;
                    let body = self.consume_expr(&args[1])?;
                    if body == Type::String {
                        Ok(http_response_type())
                    } else {
                        Err(type_error(args[1].span, &Type::String, &body))
                    }
                } else {
                    Err(KuError::runtime(
                        format!("{name} expects 1 or 2 arguments but got {}", args.len()),
                        span,
                    ))
                }
            }
            "http.json" => {
                if args.len() == 1 {
                    self.consume_expr(&args[0])?;
                    Ok(http_response_type())
                } else if args.len() == 2 {
                    self.check_http_status_arg(&args[0])?;
                    self.consume_expr(&args[1])?;
                    Ok(http_response_type())
                } else {
                    Err(KuError::runtime(
                        format!("{name} expects 1 or 2 arguments but got {}", args.len()),
                        span,
                    ))
                }
            }
            "http.empty" => {
                if args.len() > 1 {
                    return Err(KuError::runtime(
                        format!("{name} expects 0 or 1 arguments but got {}", args.len()),
                        span,
                    ));
                }
                if let Some(status) = args.first() {
                    self.check_http_status_arg(status)?;
                }
                Ok(http_response_type())
            }
            "http.redirect" => {
                if args.len() == 1 {
                    let location = self.consume_expr(&args[0])?;
                    if location == Type::String {
                        Ok(http_response_type())
                    } else {
                        Err(type_error(args[0].span, &Type::String, &location))
                    }
                } else if args.len() == 2 {
                    self.check_http_status_arg(&args[0])?;
                    let location = self.consume_expr(&args[1])?;
                    if location == Type::String {
                        Ok(http_response_type())
                    } else {
                        Err(type_error(args[1].span, &Type::String, &location))
                    }
                } else {
                    Err(KuError::runtime(
                        format!("{name} expects 1 or 2 arguments but got {}", args.len()),
                        span,
                    ))
                }
            }
            "http.statusText" => {
                expect_arg_count(name, args.len(), 1, span)?;
                self.check_http_status_arg(&args[0])?;
                Ok(Type::String)
            }
            _ => Err(KuError::runtime("invalid http helper signature", span)),
        }
    }

    fn apply_object_get_or_signature(&mut self, args: &[Expr], span: Span) -> KuResult<Type> {
        expect_arg_count("object.get_or", args.len(), 3, span)?;
        let object_type = self.check_expr(&args[0])?;
        let key_type = self.check_expr(&args[1])?;
        if !type_matches(&Type::String, &key_type) {
            return Err(type_error(args[1].span, &Type::String, &key_type));
        }
        // The default is stored into the returned value, so it is consumed.
        let default_type = self.consume_expr(&args[2])?;
        match object_type {
            // get_or on a dynamic object yields a KuValue (first-class tagged
            // value); a StringMap is homogeneous str.
            Type::Object(_) | Type::DynamicObject | Type::Unknown => Ok(Type::KuValue),
            Type::StringMap => Ok(union_or_single(vec![Type::String, default_type])),
            actual => Err(type_error(args[0].span, &Type::DynamicObject, &actual)),
        }
    }

    fn check_http_status_arg(&mut self, expr: &Expr) -> KuResult<()> {
        let actual = self.check_expr(expr)?;
        if actual == Type::Int || actual == Type::Unknown {
            Ok(())
        } else {
            Err(type_error(expr.span, &Type::Int, &actual))
        }
    }

    fn type_matches_pattern(actual: &Type, pattern: &TypePattern) -> bool {
        if let Type::Union(types) = actual {
            return types
                .iter()
                .all(|actual| Self::type_matches_pattern(actual, pattern));
        }
        match pattern {
            TypePattern::Int => actual == &Type::Int,
            TypePattern::Bool => actual == &Type::Bool,
            TypePattern::String => actual == &Type::String,
            TypePattern::Null => actual == &Type::Null,
            TypePattern::Unknown | TypePattern::Any => true,
            TypePattern::KuValue => actual == &Type::KuValue,
            TypePattern::ArrayAny => matches!(actual, Type::Array(_)),
            TypePattern::ObjectAny => matches!(
                actual,
                Type::Object(_) | Type::StringMap | Type::DynamicObject
            ),
            TypePattern::ObjectFields(fields) => match actual {
                Type::Object(actual_fields) => fields.iter().all(|(name, pattern)| {
                    actual_fields
                        .get(name)
                        .is_some_and(|actual| Self::type_matches_pattern(actual, pattern))
                }),
                _ => false,
            },
            TypePattern::StringOrStringArray => {
                actual == &Type::String || actual == &Type::Array(Box::new(Type::String))
            }
            TypePattern::ArrayOf(inner) => match actual {
                Type::Array(element) if **element == Type::Unknown => true,
                Type::Array(element) => Self::type_matches_pattern(element, inner),
                _ => false,
            },
            TypePattern::Native(name) => matches!(actual, Type::Native(n) if n == name),
            TypePattern::ArrayElementOfArg(_)
            | TypePattern::ResultOf(_)
            | TypePattern::SameAsArg(_) => true,
        }
    }

    fn pattern_expected_type(pattern: &TypePattern) -> Type {
        match pattern {
            TypePattern::Int => Type::Int,
            TypePattern::Bool => Type::Bool,
            TypePattern::String => Type::String,
            TypePattern::Null => Type::Null,
            TypePattern::ArrayAny => Type::Array(Box::new(Type::Unknown)),
            TypePattern::ObjectAny => Type::DynamicObject,
            TypePattern::ObjectFields(fields) => Type::Object(
                fields
                    .iter()
                    .map(|(name, pattern)| (name.clone(), Self::pattern_expected_type(pattern)))
                    .collect(),
            ),
            TypePattern::ArrayOf(inner) => {
                Type::Array(Box::new(Self::pattern_expected_type(inner)))
            }
            TypePattern::StringOrStringArray => Type::String,
            TypePattern::KuValue => Type::KuValue,
            TypePattern::Native(name) => Type::Native(name.to_string()),
            TypePattern::Unknown
            | TypePattern::Any
            | TypePattern::ArrayElementOfArg(_)
            | TypePattern::ResultOf(_)
            | TypePattern::SameAsArg(_) => Type::Unknown,
        }
    }

    fn stdlib_pattern_to_type(
        pattern: &TypePattern,
        actuals: &[Type],
        span: Span,
    ) -> KuResult<Type> {
        match pattern {
            TypePattern::Int => Ok(Type::Int),
            TypePattern::Bool => Ok(Type::Bool),
            TypePattern::String => Ok(Type::String),
            TypePattern::Null => Ok(Type::Null),
            TypePattern::KuValue => Ok(Type::KuValue),
            TypePattern::Native(name) => Ok(Type::Native(name.to_string())),
            TypePattern::Unknown | TypePattern::Any => Ok(Type::Unknown),
            TypePattern::ArrayAny => Ok(Type::Array(Box::new(Type::Unknown))),
            TypePattern::ObjectAny => Ok(Type::DynamicObject),
            TypePattern::ObjectFields(fields) => Ok(Type::Object(
                fields
                    .iter()
                    .map(|(name, pattern)| {
                        Ok((
                            name.clone(),
                            Self::stdlib_pattern_to_type(pattern, actuals, span)?,
                        ))
                    })
                    .collect::<KuResult<HashMap<_, _>>>()?,
            )),
            TypePattern::ArrayOf(inner) => Ok(Type::Array(Box::new(Self::stdlib_pattern_to_type(
                inner, actuals, span,
            )?))),
            TypePattern::StringOrStringArray => Ok(Type::String),
            TypePattern::ArrayElementOfArg(index) => match actuals.get(*index) {
                Some(Type::Array(element)) => Ok(*element.clone()),
                Some(actual) => Err(type_error(
                    span,
                    &Type::Array(Box::new(Type::Unknown)),
                    actual,
                )),
                None => Err(KuError::runtime("invalid stdlib signature", span)),
            },
            TypePattern::ResultOf(inner) => Ok(Type::Result(Box::new(
                Self::stdlib_pattern_to_type(inner, actuals, span)?,
            ))),
            TypePattern::SameAsArg(index) => actuals
                .get(*index)
                .cloned()
                .ok_or_else(|| KuError::runtime("invalid stdlib signature", span)),
        }
    }

    fn check_array_map_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> KuResult<Option<Type>> {
        let ExprKind::Field { target, name } = &callee.kind else {
            return Ok(None);
        };
        if name != "map" {
            return Ok(None);
        }
        expect_arg_count("array.map", args.len(), 1, span)?;
        let target_type = self.check_expr(target)?;
        let Type::Array(element) = target_type else {
            return Err(KuError::runtime(
                format!(
                    "type error: map expects array but got {}",
                    type_name(&target_type)
                ),
                target.span,
            ));
        };
        if self.type_contains_native_resource(&element) {
            return Err(KuError::runtime(
                "array.map cannot clone move-only native resource elements",
                span,
            ));
        }
        // The mapper is called with one element, so an unannotated closure
        // parameter is filled from the array's element type (the HOF context).
        let expected_mapper = Type::FunctionValue {
            params: vec![FunctionValueParam {
                name: "arg0".to_string(),
                ty: Some((*element).clone()),
            }],
            return_type: None,
            body: Vec::new(),
            body_id: None,
            is_async: false,
        };
        let mapper_type = self.check_expr_expecting(&args[0], Some(&expected_mapper))?;
        let Type::FunctionValue {
            params,
            return_type,
            body,
            body_id,
            ..
        } = mapper_type
        else {
            return Err(KuError::runtime(
                format!(
                    "type error: array.map expects function but got {}",
                    type_name(&mapper_type)
                ),
                args[0].span,
            ));
        };
        let mapped = self.check_function_value_call_with_types(
            FunctionValueBodyRef {
                params: &params,
                return_type: return_type.as_deref(),
                body: &body,
                body_id,
            },
            &[*element],
            span,
        )?;
        Ok(Some(Type::Array(Box::new(mapped))))
    }

    /// A captured binding is backed by a shared cell. Evaluating a callback
    /// argument can replace that cell after the receiver expression has been
    /// selected but before the native call executes. Copying the raw owning
    /// pointer is not a snapshot, and cloning it would duplicate ownership, so
    /// conservatively reject effectful arguments on captured move-only native
    /// receivers. Pure literals/paths remain valid.
    fn reject_effectful_args_on_captured_native_receiver(
        &self,
        target: &Expr,
        target_type: &Type,
        args: &[Expr],
        span: Span,
    ) -> KuResult<()> {
        if !matches!(target_type, Type::Native(name) if name != metadata::BYTES)
            || !args.iter().any(|arg| !is_pure_append_argument(arg, ""))
        {
            return Ok(());
        }
        let PlaceClass::Movable(place) = self.classify_place(target) else {
            return Ok(());
        };
        let captured = self
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&place.root))
            .is_some_and(|binding| binding.captured);
        if !captured {
            return Ok(());
        }
        Err(KuError::runtime(
            format!(
                "cannot call a move-only native receiver rooted at '{}' with an effectful argument after that binding was captured; evaluate the argument into a local first",
                place.root
            ),
            span,
        ))
    }

    fn check_function_value_call(
        &mut self,
        function: FunctionValueBodyRef<'_>,
        args: &[Expr],
        span: Span,
        name: Option<&str>,
    ) -> KuResult<(Type, Vec<Type>)> {
        if function.params.len() != args.len() {
            let subject = name
                .map(|name| format!("function value '{name}'"))
                .unwrap_or_else(|| "function value".to_string());
            return Err(KuError::runtime(
                format!(
                    "{subject} expects {} arguments but got {}",
                    function.params.len(),
                    args.len()
                ),
                span,
            ));
        }
        let actual_arg_types = args
            .iter()
            .zip(function.params.iter())
            .map(|(arg, param)| self.consume_arg_expr_expecting(arg, param.ty.as_ref()))
            .collect::<KuResult<Vec<_>>>()?;
        let mut arg_types = Vec::new();
        for ((param, actual), arg) in function
            .params
            .iter()
            .zip(actual_arg_types.iter())
            .zip(args.iter())
        {
            if let Some(expected) = &param.ty {
                if !type_matches(expected, actual) {
                    return Err(type_error(arg.span, expected, actual));
                }
                if self.readonly_capture.is_some() && matches!(actual, Type::FunctionValue { .. }) {
                    // Preserve the concrete body through a typed higher-order
                    // parameter so the handler audit can follow the call instead
                    // of seeing only the annotation's empty signature body.
                    arg_types.push(actual.clone());
                } else {
                    arg_types.push(expected.clone());
                }
            } else {
                arg_types.push(actual.clone());
            }
        }
        let returns = self.check_function_value_call_with_types(function, &arg_types, span)?;
        Ok((returns, actual_arg_types))
    }

    fn check_function_value_call_with_types(
        &mut self,
        function: FunctionValueBodyRef<'_>,
        arg_types: &[Type],
        span: Span,
    ) -> KuResult<Type> {
        self.check_function_value_call_with_types_inner(function, arg_types, span, None)
    }

    fn check_function_value_call_with_types_readonly_captures(
        &mut self,
        function: FunctionValueBodyRef<'_>,
        arg_types: &[Type],
        span: Span,
    ) -> KuResult<Type> {
        self.check_function_value_call_with_types_inner(
            function,
            arg_types,
            span,
            Some("http handler"),
        )
    }

    fn check_function_value_call_with_types_inner(
        &mut self,
        function: FunctionValueBodyRef<'_>,
        arg_types: &[Type],
        span: Span,
        readonly_capture_owner: Option<&'static str>,
    ) -> KuResult<Type> {
        if function.params.len() != arg_types.len() {
            return Err(KuError::runtime(
                format!(
                    "function value expects {} arguments but got {}",
                    function.params.len(),
                    arg_types.len()
                ),
                span,
            ));
        }
        for (param, actual) in function.params.iter().zip(arg_types.iter()) {
            if let Some(expected) = &param.ty {
                if !type_matches(expected, actual) {
                    return Err(type_error(span, expected, actual));
                }
            }
        }
        if let Some(owner) = readonly_capture_owner {
            self.check_function_value_body_readonly_captures(function, arg_types, span, owner)
        } else {
            // An annotated FunctionValue is normally already checked. When a
            // read-only handler/task audit is active, its body must still be
            // traversed because it may mutate bindings captured outside the
            // outer execution boundary.
            if self.readonly_capture.is_none() {
                if let Some(return_type) = function.return_type {
                    return Ok(return_type.clone());
                }
            }
            self.check_function_value_body(
                function.params,
                function.return_type,
                function.body,
                function.body_id,
                arg_types,
                span,
            )
        }
    }

    fn check_function_value_body(
        &mut self,
        params: &[FunctionValueParam],
        return_type: Option<&Type>,
        body: &[Stmt],
        body_id: Option<FunctionBodyId>,
        arg_types: &[Type],
        span: Span,
    ) -> KuResult<Type> {
        let guard_inference = return_type.is_none();
        if guard_inference {
            let Some(body_id) = body_id else {
                return Ok(Type::Unknown);
            };
            if self.function_value_inference_stack.contains(&body_id) {
                return Ok(Type::Unknown);
            }
        }
        let readonly_body_key = if let Some(capture) = self.readonly_capture {
            if capture.owner == "http handler" && (body.is_empty() || body_id.is_none()) {
                return Err(KuError::runtime(
                    "http handler cannot prove a captured function value is read-only because its body is unavailable",
                    span,
                ));
            }
            if let Some(body_id) = body_id {
                if self.readonly_function_body_stack.contains(&body_id) {
                    return Ok(return_type.cloned().unwrap_or(Type::Unknown));
                }
                self.readonly_function_body_stack.push(body_id);
                Some(body_id)
            } else {
                None
            }
        } else {
            None
        };
        let inference_body_key = guard_inference.then_some(body_id).flatten();
        if let Some(body_id) = inference_body_key {
            self.function_value_inference_stack.push(body_id);
        }
        let saved_return = self.current_return.clone();
        let saved_loop_depth = self.loop_depth;
        let saved_recoverable_depth = self.recoverable_depth;
        // A closure executes later; its return/fail/? edges do not enter a try
        // surrounding the closure literal's creation.
        let saved_try_exit_collectors = std::mem::take(&mut self.try_exit_collectors);
        self.current_return = return_type.cloned().unwrap_or(Type::Unknown);
        self.loop_depth = 0;
        self.recoverable_depth = 0;
        self.push_scope();
        // Stage 6c-str: everything defined at or above this scope index is local to
        // the closure body; anything below it is captured from an enclosing scope
        // and may not be moved out (E0904).
        self.closure_capture_boundaries.push(self.scopes.len() - 1);

        let result = (|| -> KuResult<Type> {
            for (param, ty) in params.iter().zip(arg_types.iter()) {
                self.define(param.name.clone(), ty.clone(), false, span)?;
            }

            let mut inferred_return = Type::Null;
            for stmt in body {
                if let Some(return_type) = self.check_stmt_and_infer_return(stmt)? {
                    inferred_return = merge_return_types(&inferred_return, &return_type, span)?;
                }
            }
            if let Some(expected) = return_type {
                if expected != &Type::Void && !block_may_return(body) {
                    return Err(KuError::runtime(
                        format!("function value must return {}", type_name(expected)),
                        span,
                    ));
                }
                if inferred_return != Type::Null && !type_matches(expected, &inferred_return) {
                    return Err(type_error(span, expected, &inferred_return));
                }
            }
            Ok(inferred_return)
        })();

        // Every enclosing binding this closure reads is boxed into a shared cell
        // that the closure loads from on each call. Moving such a binding in the
        // outer scope empties the cell (the backend moves out of `(cell)->value`),
        // so mark them and let `record_move` require an explicit clone instead.
        if let Some(&boundary) = self.closure_capture_boundaries.last() {
            let captured: Vec<String> = self.scopes[..boundary]
                .iter()
                .flat_map(|scope| scope.keys().cloned())
                .filter(|name| function_body_uses_name(body, name))
                .collect();
            for scope in self.scopes[..boundary].iter_mut() {
                for name in &captured {
                    if let Some(var) = scope.get_mut(name) {
                        var.captured = true;
                    }
                }
            }
        }
        self.closure_capture_boundaries.pop();
        self.pop_scope();
        self.current_return = saved_return;
        self.loop_depth = saved_loop_depth;
        self.recoverable_depth = saved_recoverable_depth;
        self.try_exit_collectors = saved_try_exit_collectors;
        if inference_body_key.is_some() {
            self.function_value_inference_stack.pop();
        }
        if let Some(expected_key) = readonly_body_key {
            let popped = self.readonly_function_body_stack.pop();
            debug_assert_eq!(popped, Some(expected_key));
        }
        result
    }

    fn check_function_value_body_readonly_captures(
        &mut self,
        function: FunctionValueBodyRef<'_>,
        arg_types: &[Type],
        span: Span,
        owner: &'static str,
    ) -> KuResult<Type> {
        let saved_capture = self.readonly_capture;
        let effective_capture = saved_capture.unwrap_or(ReadonlyCapture {
            // This is the index of the function scope pushed below. If a
            // boundary already exists, keep it so nested bodies inherit the
            // outermost handler/task execution boundary.
            boundary: self.scopes.len(),
            owner,
        });
        if effective_capture.owner == "http handler"
            && (function.body.is_empty() || function.body_id.is_none())
        {
            return Err(KuError::runtime(
                "http handler cannot prove a function value is read-only because its body is unavailable",
                span,
            ));
        }
        if let Some(body_id) = function.body_id {
            if self.readonly_function_body_stack.contains(&body_id) {
                return Ok(function.return_type.cloned().unwrap_or(Type::Unknown));
            }
            self.readonly_function_body_stack.push(body_id);
        }
        self.push_scope();
        self.readonly_capture = Some(effective_capture);
        let saved_return = self.current_return.clone();
        let saved_loop_depth = self.loop_depth;
        let saved_recoverable_depth = self.recoverable_depth;
        // Handler/async bodies are separate executions too; do not report their
        // abrupt exits to a try active where the function value is checked.
        let saved_try_exit_collectors = std::mem::take(&mut self.try_exit_collectors);
        self.current_return = function.return_type.cloned().unwrap_or(Type::Unknown);
        self.loop_depth = 0;
        self.recoverable_depth = 0;

        let result = (|| -> KuResult<Type> {
            for (param, ty) in function.params.iter().zip(arg_types.iter()) {
                self.define(param.name.clone(), ty.clone(), false, span)?;
                // An HTTP handler's request is a native struct in the IR, so its
                // fields are movable individually -- `http.text(req.body)` is the
                // idiomatic handler body. Keying on `owner` keeps a user object of
                // the same shape (passed to an async task) out of this.
                if owner == "http handler" && *ty == http_request_type() {
                    if let Some(scope) = self.scopes.last_mut() {
                        if let Some(var) = scope.get_mut(&param.name) {
                            var.struct_backed = true;
                        }
                    }
                }
            }

            let mut inferred_return = Type::Null;
            for stmt in function.body {
                if let Some(return_type) = self.check_stmt_and_infer_return(stmt)? {
                    inferred_return = merge_return_types(&inferred_return, &return_type, span)?;
                }
            }
            if let Some(expected) = function.return_type {
                if expected != &Type::Void && !block_may_return(function.body) {
                    return Err(KuError::runtime(
                        format!("function value must return {}", type_name(expected)),
                        span,
                    ));
                }
                if inferred_return != Type::Null && !type_matches(expected, &inferred_return) {
                    return Err(type_error(span, expected, &inferred_return));
                }
            }
            Ok(inferred_return)
        })();

        self.current_return = saved_return;
        self.loop_depth = saved_loop_depth;
        self.recoverable_depth = saved_recoverable_depth;
        self.try_exit_collectors = saved_try_exit_collectors;
        self.readonly_capture = saved_capture;
        self.pop_scope();
        if let Some(expected_body_id) = function.body_id {
            let popped = self.readonly_function_body_stack.pop();
            debug_assert_eq!(popped, Some(expected_body_id));
        }
        result
    }

    fn check_local_function(&mut self, function: &FnDecl) -> KuResult<()> {
        reject_duplicate_params(function)?;
        let is_async = function.is_async;
        if is_async {
            self.require_async_result_return(function)?;
        }
        let params = function
            .params
            .iter()
            .map(|param| {
                Ok(FunctionValueParam {
                    name: param.name.clone(),
                    ty: param
                        .ty
                        .as_ref()
                        .map(|ty| {
                            self.resolve_type_name_with_generics(
                                ty,
                                param.span,
                                &function.type_params,
                            )
                        })
                        .transpose()?,
                })
            })
            .collect::<KuResult<Vec<_>>>()?;
        let return_type = function
            .return_type
            .as_ref()
            .map(|ty| {
                self.resolve_type_name_with_generics(ty, function.span, &function.type_params)
            })
            .transpose()?;
        let body_id = self.fresh_function_body_id();
        let visible_names = self
            .visible_binding_ids()
            .into_keys()
            .collect::<HashSet<_>>();
        let captured_names = checker_local_function_capture_names(function, &visible_names);
        self.record_function_body_outer_bindings(body_id, &captured_names);
        self.define(
            function.name.clone(),
            Type::FunctionValue {
                params: params.clone(),
                return_type: return_type.clone().map(Box::new),
                body: function.body.clone(),
                body_id: Some(body_id),
                is_async,
            },
            false,
            function.span,
        )?;
        let mut closure_provenance = ClosureProvenance::empty();
        for binding_id in self
            .function_body_outer_bindings
            .get(&body_id)
            .into_iter()
            .flat_map(|bindings| bindings.values())
        {
            closure_provenance.dependencies.insert(*binding_id);
        }
        self.set_closure_provenance(&function.name, closure_provenance, function.span)?;
        let arg_types = params
            .iter()
            .map(|param| param.ty.clone().unwrap_or(Type::Unknown))
            .collect::<Vec<_>>();
        let saved_async_depth = self.async_depth;
        self.async_depth = usize::from(is_async);
        let result = if is_async {
            self.check_function_value_body_readonly_captures(
                FunctionValueBodyRef {
                    params: &params,
                    return_type: return_type.as_ref(),
                    body: &function.body,
                    body_id: Some(body_id),
                },
                &arg_types,
                function.span,
                "async task",
            )
        } else {
            self.check_function_value_body(
                &params,
                return_type.as_ref(),
                &function.body,
                Some(body_id),
                &arg_types,
                function.span,
            )
        };
        self.async_depth = saved_async_depth;
        result.map(|_| ())
    }

    fn require_async_result_return(&self, function: &FnDecl) -> KuResult<()> {
        let Some(return_type) = &function.return_type else {
            return Err(KuError::runtime(
                format!(
                    "async fn '{}' must explicitly declare a Result return type such as T!",
                    function.name
                ),
                function.span,
            ));
        };
        let resolved = self.resolve_type_name_with_generics(
            return_type,
            function.span,
            &function.type_params,
        )?;
        if !matches!(resolved, Type::Result(_)) {
            return Err(KuError::runtime(
                format!(
                    "async fn '{}' must return T!, got {}",
                    function.name,
                    type_name(&resolved)
                ),
                function.span,
            ));
        }
        Ok(())
    }

    fn check_stmt_and_infer_return(&mut self, stmt: &Stmt) -> KuResult<Option<Type>> {
        match stmt {
            Stmt::Return { value, span } => {
                let expected = self.current_return.clone();
                let actual = match value {
                    // Returning an owned value moves it out; consuming here lets the
                    // closure-body move checks (e.g. E0904 for a captured owned
                    // value) fire, matching the top-level `return` path.
                    Some(value) => self.consume_expr_expecting(value, Some(&expected))?,
                    None if self.current_return == Type::Void => Type::Void,
                    None => Type::Null,
                };
                if !type_matches(&self.current_return, &actual) {
                    return Err(type_error(*span, &self.current_return, &actual));
                }
                self.capture_try_exit(TryExitKind::Return);
                Ok(Some(actual))
            }
            Stmt::Fail { value, span } => {
                let actual = self.consume_expr(value)?;
                if actual != Type::String && !matches!(actual, Type::Object(_)) {
                    return Err(type_error(*span, &error_type(), &actual));
                }
                if !matches!(self.current_return, Type::Result(_)) {
                    if self.recoverable_depth > 0 {
                        self.capture_try_exit(TryExitKind::Throw);
                        return Ok(None);
                    }
                    return Err(KuError::runtime(
                        format!(
                            "fail requires a Result return type or an enclosing try block, got {}",
                            type_name(&self.current_return)
                        ),
                        *span,
                    ));
                }
                self.capture_try_exit(TryExitKind::Throw);
                Ok(Some(self.current_return.clone()))
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                self.expect_condition(condition, *span)?;
                // Check each branch from the pre-if move state, then merge the two
                // resulting states per field path (`join_move_marks`). Without this
                // the then-branch's moves would leak into the else-branch and past
                // the `if`, and a path moved on only one branch would look
                // definitely-moved instead of `MaybeMoved`.
                let before = self.scopes.clone();
                let then_return = self.check_block_and_infer_return(then_branch)?;
                let then_scopes = self.scopes.clone();
                self.scopes = before.clone();
                let else_return = self.check_block_and_infer_return(else_branch)?;
                let else_scopes = self.scopes.clone();
                let then_falls = !block_stops_fallthrough(then_branch);
                let else_falls = !block_stops_fallthrough(else_branch);
                self.scopes =
                    merge_if_scopes(before, then_scopes, else_scopes, then_falls, else_falls);
                match (then_return, else_return) {
                    (Some(left), Some(right)) => {
                        Ok(Some(merge_return_types(&left, &right, *span)?))
                    }
                    (Some(left), None) | (None, Some(left)) => Ok(Some(left)),
                    (None, None) => Ok(None),
                }
            }
            Stmt::Break { span } => {
                if self.loop_depth == 0 {
                    Err(KuError::runtime("break outside loop", *span))
                } else {
                    Ok(None)
                }
            }
            Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    Err(KuError::runtime("continue outside loop", *span))
                } else {
                    Ok(None)
                }
            }
            _ => {
                self.check_stmt(stmt)?;
                Ok(None)
            }
        }
    }

    fn check_block_and_infer_return(&mut self, body: &[Stmt]) -> KuResult<Option<Type>> {
        self.push_scope();
        let mut inferred = None;
        for stmt in body {
            if let Some(return_type) = self.check_stmt_and_infer_return(stmt)? {
                inferred = Some(match inferred {
                    Some(existing) => merge_return_types(&existing, &return_type, stmt_span(stmt))?,
                    None => return_type,
                });
            }
        }
        self.pop_scope();
        Ok(inferred)
    }

    fn expect_condition(&mut self, expr: &Expr, span: Span) -> KuResult<()> {
        let ty = self.check_expr(expr)?;
        if ty == Type::Bool {
            Ok(())
        } else {
            Err(KuError::runtime(
                format!(
                    "type error: condition must be bool but got {}",
                    type_name(&ty)
                ),
                span,
            ))
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, ty: Type, mutable: bool, span: Span) -> KuResult<()> {
        reject_reserved_name(&name, span)?;
        if self
            .scopes
            .last()
            .expect("checker always has a scope")
            .contains_key(&name)
        {
            return Err(KuError::runtime(
                format!("variable '{name}' is already defined in this scope"),
                span,
            ));
        }
        let binding_id = self.fresh_binding_id();
        self.scopes
            .last_mut()
            .expect("checker always has a scope")
            .insert(
                name,
                VarType::live(binding_id, ty, mutable, ClosureProvenance::unknown()),
            );
        Ok(())
    }

    fn contains(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.contains_key(name))
    }

    fn get(&self, name: &str, span: Span) -> KuResult<VarType> {
        for scope in self.scopes.iter().rev() {
            if let Some(var) = scope.get(name) {
                // Whole-variable move only. A partially-moved variable can still be
                // projected into for its live fields (`user.age` after `user.name`
                // moved), so the partial read-block is enforced at the value node,
                // not here where field-projection bases are also resolved.
                if var.whole_move().is_some() {
                    return Err(KuError::runtime(
                        format!(
                            "use of moved value '{name}'; call '{name}.clone()' before moving when an explicit copy is required"
                        ),
                        span,
                    ));
                }
                return Ok(var.clone());
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
    }

    fn get_allow_moved(&self, name: &str, span: Span) -> KuResult<VarType> {
        for scope in self.scopes.iter().rev() {
            if let Some(var) = scope.get(name) {
                return Ok(var.clone());
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
    }

    fn set_closure_provenance(
        &mut self,
        name: &str,
        provenance: ClosureProvenance,
        span: Span,
    ) -> KuResult<()> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(var) = scope.get_mut(name) {
                var.closure_provenance = provenance;
                return Ok(());
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
    }

    fn merge_closure_provenance(
        &mut self,
        name: &str,
        provenance: &ClosureProvenance,
        span: Span,
    ) -> KuResult<()> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(var) = scope.get_mut(name) {
                var.closure_provenance.merge(provenance);
                return Ok(());
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
    }

    fn update_function_value_binding_type(&mut self, name: &str, actual: &Type) {
        if !matches!(actual, Type::FunctionValue { .. }) {
            return;
        }
        for scope in self.scopes.iter_mut().rev() {
            if let Some(var) = scope.get_mut(name) {
                if matches!(var.ty, Type::FunctionValue { .. }) {
                    var.ty = actual.clone();
                }
                return;
            }
        }
    }

    fn binding_by_id(&self, binding_id: BindingId) -> Option<&VarType> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.values())
            .find(|var| var.binding_id == binding_id)
    }

    fn reject_closure_reference_cycle(
        &self,
        target: &str,
        provenance: &ClosureProvenance,
        span: Span,
    ) -> KuResult<()> {
        let target_binding = self.get_allow_moved(target, span)?;
        let mut visited = HashSet::new();
        let creates_cycle = provenance.dependencies.iter().copied().any(|dependency| {
            self.closure_dependency_reaches(dependency, target_binding.binding_id, &mut visited)
        });
        if creates_cycle {
            return Err(KuError::runtime(
                format!(
                    "E0904 cannot create closure reference cycle involving '{target}'; use a named local function for recursion or break the captured ownership path"
                ),
                span,
            ));
        }
        Ok(())
    }

    fn closure_dependency_reaches(
        &self,
        current: BindingId,
        target: BindingId,
        visited: &mut HashSet<BindingId>,
    ) -> bool {
        if current == target {
            return true;
        }
        if !visited.insert(current) {
            return false;
        }
        self.binding_by_id(current).is_some_and(|binding| {
            binding
                .closure_provenance
                .dependencies
                .iter()
                .copied()
                .any(|dependency| self.closure_dependency_reaches(dependency, target, visited))
        })
    }

    fn expression_closure_provenance(&self, expr: &Expr) -> ClosureProvenance {
        self.expression_closure_provenance_inner(
            expr,
            &HashMap::new(),
            &mut ClosureSummaryContext::new(),
            true,
        )
    }

    fn expression_closure_provenance_inner(
        &self,
        expr: &Expr,
        symbolic: &HashMap<String, ClosureProvenance>,
        summaries: &mut ClosureSummaryContext,
        allow_checker_bindings: bool,
    ) -> ClosureProvenance {
        match &expr.kind {
            ExprKind::Literal(_) | ExprKind::Unary { .. } | ExprKind::Binary { .. } => {
                ClosureProvenance::empty()
            }
            ExprKind::Variable(name) => {
                if let Some(provenance) = symbolic.get(name) {
                    return provenance.clone();
                }
                if allow_checker_bindings {
                    if let Ok(binding) = self.get_allow_moved(name, expr.span) {
                        return binding.closure_provenance;
                    }
                }
                if self.functions.contains_key(name) {
                    return ClosureProvenance::empty();
                }
                ClosureProvenance::unknown()
            }
            ExprKind::Function { params, body, .. } => {
                let mut provenance = ClosureProvenance::empty();
                let mut visible_names = symbolic.keys().cloned().collect::<HashSet<_>>();
                if allow_checker_bindings {
                    visible_names.extend(self.visible_binding_ids().into_keys());
                }
                for name in checker_closure_capture_names(params, body, &visible_names) {
                    if let Some(captured) = symbolic.get(&name) {
                        provenance.merge(captured);
                    } else if allow_checker_bindings {
                        if let Ok(binding) = self.get_allow_moved(&name, expr.span) {
                            provenance.dependencies.insert(binding.binding_id);
                        } else if !self.functions.contains_key(&name) {
                            provenance.complete = false;
                        }
                    } else if !self.functions.contains_key(&name) {
                        provenance.complete = false;
                    }
                }
                provenance
            }
            ExprKind::Array(values) => {
                let mut provenance = ClosureProvenance::empty();
                for value in values {
                    provenance.merge(&self.expression_closure_provenance_inner(
                        value,
                        symbolic,
                        summaries,
                        allow_checker_bindings,
                    ));
                }
                provenance
            }
            ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
                let mut provenance = ClosureProvenance::empty();
                for (_, value) in fields {
                    provenance.merge(&self.expression_closure_provenance_inner(
                        value,
                        symbolic,
                        summaries,
                        allow_checker_bindings,
                    ));
                }
                provenance
            }
            ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => self
                .expression_closure_provenance_inner(
                    expr,
                    symbolic,
                    summaries,
                    allow_checker_bindings,
                ),
            ExprKind::Match { value, arms } => {
                let selected = self.expression_closure_provenance_inner(
                    value,
                    symbolic,
                    summaries,
                    allow_checker_bindings,
                );
                let mut provenance = ClosureProvenance::empty();
                for arm in arms {
                    let mut arm_symbolic = symbolic.clone();
                    bind_match_pattern_closure_provenance(
                        &arm.pattern,
                        &selected,
                        &mut arm_symbolic,
                    );
                    provenance.merge(&self.expression_closure_provenance_inner(
                        &arm.value,
                        &arm_symbolic,
                        summaries,
                        allow_checker_bindings,
                    ));
                }
                provenance
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { target, name } = &callee.kind {
                    if name == "clone" && args.is_empty() {
                        return self.expression_closure_provenance_inner(
                            target,
                            symbolic,
                            summaries,
                            allow_checker_bindings,
                        );
                    }
                }
                let argument_provenance = args
                    .iter()
                    .map(|arg| {
                        self.expression_closure_provenance_inner(
                            arg,
                            symbolic,
                            summaries,
                            allow_checker_bindings,
                        )
                    })
                    .collect::<Vec<_>>();
                if let ExprKind::Variable(name) = &callee.kind {
                    if let Some(function) = self.functions.get(name) {
                        return self.known_function_body_return_provenance(
                            ClosureBodyView {
                                params: &function.value_params,
                                body: &function.body,
                                body_id: function.body_id,
                            },
                            &function.params,
                            &argument_provenance,
                            &ClosureProvenance::empty(),
                            summaries,
                        );
                    }
                    if allow_checker_bindings {
                        if let Ok(binding) = self.get_allow_moved(name, callee.span) {
                            if let Type::FunctionValue {
                                params,
                                body,
                                body_id: Some(body_id),
                                ..
                            } = &binding.ty
                            {
                                if !body.is_empty() {
                                    let parameter_types = params
                                        .iter()
                                        .map(|param| param.ty.clone().unwrap_or(Type::Unknown))
                                        .collect::<Vec<_>>();
                                    return self.known_function_body_return_provenance(
                                        ClosureBodyView {
                                            params,
                                            body,
                                            body_id: *body_id,
                                        },
                                        &parameter_types,
                                        &argument_provenance,
                                        &binding.closure_provenance,
                                        summaries,
                                    );
                                }
                            }
                        }
                    }
                }
                let mut unknown = self.expression_closure_provenance_inner(
                    callee,
                    symbolic,
                    summaries,
                    allow_checker_bindings,
                );
                for argument in &argument_provenance {
                    unknown.merge(argument);
                }
                unknown.complete = false;
                unknown
            }
            // Until provenance is field/index-sensitive, preserve every known
            // container dependency when selecting a member. Mark it incomplete so
            // callers do not mistake the union for an exact selected-field value.
            ExprKind::Index { target, .. }
            | ExprKind::Field { target, .. }
            | ExprKind::OptionalField { target, .. } => {
                let mut provenance = self.expression_closure_provenance_inner(
                    target,
                    symbolic,
                    summaries,
                    allow_checker_bindings,
                );
                provenance.complete = false;
                provenance
            }
        }
    }

    fn known_function_body_return_provenance(
        &self,
        function: ClosureBodyView<'_>,
        parameter_types: &[Type],
        arguments: &[ClosureProvenance],
        captured_environment: &ClosureProvenance,
        summaries: &mut ClosureSummaryContext,
    ) -> ClosureProvenance {
        let ClosureBodyView {
            params,
            body,
            body_id,
        } = function;
        if params.len() != arguments.len() {
            return ClosureProvenance::unknown();
        }

        let key = ClosureReturnSummaryKey {
            body_id,
            captured_environment: captured_environment.into(),
            arguments: arguments.iter().map(Into::into).collect(),
        };
        if let Some(cached) = summaries.return_cache.get(&key) {
            return cached.clone();
        }

        let conservative = || {
            let mut unknown = captured_environment.clone();
            for (parameter_type, argument) in parameter_types.iter().zip(arguments) {
                if self.type_may_contain_function_value(parameter_type) {
                    unknown.merge(argument);
                }
            }
            unknown.complete = false;
            unknown
        };
        if summaries.remaining_states == 0 || !summaries.active_bodies.insert(body_id) {
            return conservative();
        }
        summaries.remaining_states -= 1;

        // A concrete local FunctionValue carries an environment. Seed every
        // lexically-free name with that environment's aggregate dependency set;
        // parameters then override it. This preserves returned closures that
        // capture an outer binding, and remains valid when the function value was
        // cloned or passed through an alias whose original lexical cells are no
        // longer present in the checker's current scope.
        let mut symbolic = self
            .function_body_outer_bindings
            .get(&body_id)
            .into_iter()
            .flat_map(|bindings| bindings.keys().cloned())
            .map(|name| (name, captured_environment.clone()))
            .collect::<HashMap<_, _>>();
        symbolic.extend(
            params
                .iter()
                .zip(arguments)
                .map(|(param, provenance)| (param.name.clone(), provenance.clone())),
        );
        let flow = self.function_return_provenance_flow(body, symbolic, summaries);
        summaries.active_bodies.remove(&body_id);
        let had_return = flow.returned.is_some();
        let mut result = flow.returned.unwrap_or_else(ClosureProvenance::unknown);
        let fully_proven =
            flow.complete && flow.fallthrough.is_none() && had_return && result.complete;
        if !fully_proven {
            // An incomplete concrete summary is not treated as empty. Preserve
            // every function-owning argument as a possible returned dependency;
            // a proven straight-line Discard remains complete and avoids this.
            for (parameter_type, argument) in parameter_types.iter().zip(arguments) {
                if self.type_may_contain_function_value(parameter_type) {
                    result.merge(argument);
                }
            }
            result.merge(captured_environment);
            result.complete = false;
        }
        summaries.return_cache.insert(key, result.clone());
        result
    }

    fn function_return_provenance_flow(
        &self,
        body: &[Stmt],
        mut symbolic: HashMap<String, ClosureProvenance>,
        summaries: &mut ClosureSummaryContext,
    ) -> ClosureReturnFlow {
        let mut returned = None;
        let mut complete = true;
        for stmt in body {
            match stmt {
                Stmt::VarDecl { name, value, .. } => {
                    let provenance = self
                        .expression_closure_provenance_inner(value, &symbolic, summaries, false);
                    symbolic.insert(name.clone(), provenance);
                }
                Stmt::Assign { name, value, .. } => {
                    if symbolic.contains_key(name) {
                        let provenance = self.expression_closure_provenance_inner(
                            value, &symbolic, summaries, false,
                        );
                        symbolic.insert(name.clone(), provenance);
                    } else {
                        complete = false;
                    }
                }
                Stmt::Return { value, .. } => {
                    let provenance =
                        value
                            .as_ref()
                            .map_or_else(ClosureProvenance::empty, |value| {
                                self.expression_closure_provenance_inner(
                                    value, &symbolic, summaries, false,
                                )
                            });
                    merge_optional_closure_provenance(&mut returned, Some(provenance));
                    return ClosureReturnFlow {
                        returned,
                        fallthrough: None,
                        complete,
                    };
                }
                Stmt::If {
                    then_branch,
                    else_branch,
                    ..
                } => {
                    let then_flow = restore_symbolic_block_scope(
                        self.function_return_provenance_flow(
                            then_branch,
                            symbolic.clone(),
                            summaries,
                        ),
                        &symbolic,
                        then_branch,
                    );
                    let else_flow = if else_branch.is_empty() {
                        ClosureReturnFlow {
                            returned: None,
                            fallthrough: Some(symbolic.clone()),
                            complete: true,
                        }
                    } else {
                        restore_symbolic_block_scope(
                            self.function_return_provenance_flow(
                                else_branch,
                                symbolic.clone(),
                                summaries,
                            ),
                            &symbolic,
                            else_branch,
                        )
                    };
                    merge_optional_closure_provenance(&mut returned, then_flow.returned);
                    merge_optional_closure_provenance(&mut returned, else_flow.returned);
                    complete &= then_flow.complete && else_flow.complete;
                    match merge_symbolic_fallthrough(then_flow.fallthrough, else_flow.fallthrough) {
                        Some(merged) => symbolic = merged,
                        None => {
                            return ClosureReturnFlow {
                                returned,
                                fallthrough: None,
                                complete,
                            };
                        }
                    }
                }
                Stmt::Expr { expr, .. } if expr_may_call_function(expr) => complete = false,
                Stmt::Print { value, .. } if expr_may_call_function(value) => complete = false,
                Stmt::Expr { .. } | Stmt::Print { .. } => {}
                Stmt::Fail { .. }
                | Stmt::Panic { .. }
                | Stmt::Break { .. }
                | Stmt::Continue { .. } => {
                    return ClosureReturnFlow {
                        returned,
                        fallthrough: None,
                        complete,
                    };
                }
                // Loops, try/finally and mutation through projections need a
                // richer flow model. Mark the summary incomplete so the caller
                // conservatively propagates function-owning arguments.
                _ => complete = false,
            }
        }
        ClosureReturnFlow {
            returned,
            fallthrough: Some(symbolic),
            complete,
        }
    }

    fn apply_known_function_closure_effects(
        &mut self,
        callee_name: &str,
        function: ClosureBodyView<'_>,
        arguments: &[ClosureProvenance],
        argument_types: &[Type],
        span: Span,
    ) -> KuResult<()> {
        let ClosureBodyView {
            params,
            body,
            body_id,
        } = function;
        if params.len() != arguments.len()
            || params.len() != argument_types.len()
            || body.is_empty()
        {
            return Ok(());
        }
        let callee = self.get_allow_moved(callee_name, span)?;
        let mut summaries = ClosureSummaryContext::new();
        let mut summary = self.known_function_body_effect_summary(
            ClosureBodyView {
                params,
                body,
                body_id,
            },
            arguments,
            argument_types,
            &callee.closure_provenance,
            &mut summaries,
        );
        if !summary.complete {
            // Unsupported calls/loops must never erase known ownership. Retain
            // the callee environment and every function-capable argument as a
            // possible write to each captured function-capable cell.
            let mut conservative = callee.closure_provenance.clone();
            for (param, argument) in params.iter().zip(arguments) {
                if param
                    .ty
                    .as_ref()
                    .is_none_or(|ty| self.type_may_contain_function_value(ty))
                {
                    conservative.merge(argument);
                }
            }
            conservative.complete = false;
            for target in self
                .function_body_outer_bindings
                .get(&body_id)
                .into_iter()
                .flat_map(|bindings| bindings.values().copied())
            {
                if self
                    .binding_by_id(target)
                    .is_some_and(|binding| self.type_may_contain_function_value(&binding.ty))
                {
                    let mut provenance = conservative.clone();
                    // Merely owning the target cell is not itself a write-back;
                    // remove that tautological edge so an opaque but harmless
                    // call does not immediately self-cycle its callee capture.
                    provenance.dependencies.remove(&target);
                    summary
                        .effects
                        .push(ClosureWriteEffect { target, provenance });
                }
            }
        }

        let mut environment_effect = ClosureProvenance::empty();
        for effect in summary.effects {
            environment_effect.merge(&effect.provenance);
            let Some((target_name, target_type)) = self.binding_name_and_type_by_id(effect.target)
            else {
                continue;
            };
            if !self.type_may_contain_function_value(&target_type) {
                continue;
            }
            self.reject_closure_reference_cycle(&target_name, &effect.provenance, span)?;
            self.merge_closure_provenance(&target_name, &effect.provenance, span)?;
        }
        if !environment_effect.dependencies.is_empty() || !environment_effect.complete {
            // A FunctionValue owns every cell in its environment. A write through
            // any captured cell therefore updates what aliases of this callee can
            // reach; retaining the aggregate edge also catches hidden cells whose
            // lexical binding has left the checker scope.
            self.reject_closure_reference_cycle(callee_name, &environment_effect, span)?;
            self.merge_closure_provenance(callee_name, &environment_effect, span)?;
        }
        Ok(())
    }

    fn apply_top_level_function_closure_effects(
        &mut self,
        params: &[FunctionValueParam],
        body: &[Stmt],
        body_id: FunctionBodyId,
        argument_types: &[Type],
        arguments: &[ClosureProvenance],
        span: Span,
    ) -> KuResult<()> {
        if params.len() != arguments.len()
            || params.len() != argument_types.len()
            || body.is_empty()
        {
            return Ok(());
        }
        let mut summaries = ClosureSummaryContext::new();
        let mut summary = self.known_function_body_effect_summary(
            ClosureBodyView {
                params,
                body,
                body_id,
            },
            arguments,
            argument_types,
            &ClosureProvenance::empty(),
            &mut summaries,
        );
        if !summary.complete {
            // If a wrapper exceeded the bounded analysis or contains an opaque
            // call, a concrete callable argument may still write through any
            // cell it captures. Preserve those possible targets and all
            // function-owning argument dependencies. A proven Discard body
            // remains complete and never enters this fallback.
            let mut conservative = ClosureProvenance::empty();
            for (ty, argument) in argument_types.iter().zip(arguments) {
                if self.type_may_contain_function_value(ty) {
                    conservative.merge(argument);
                }
            }
            conservative.complete = false;
            let possible_targets = argument_types
                .iter()
                .zip(arguments)
                .filter(|(ty, _)| matches!(ty, Type::FunctionValue { .. }))
                .flat_map(|(_, argument)| argument.dependencies.iter().copied())
                .collect::<HashSet<_>>();
            for target in possible_targets {
                if self
                    .binding_by_id(target)
                    .is_some_and(|binding| self.type_may_contain_function_value(&binding.ty))
                {
                    summary.effects.push(ClosureWriteEffect {
                        target,
                        provenance: conservative.clone(),
                    });
                }
            }
        }

        for effect in summary.effects {
            let Some((target_name, target_type)) = self.binding_name_and_type_by_id(effect.target)
            else {
                continue;
            };
            if !self.type_may_contain_function_value(&target_type) {
                continue;
            }
            self.reject_closure_reference_cycle(&target_name, &effect.provenance, span)?;
            self.merge_closure_provenance(&target_name, &effect.provenance, span)?;
        }
        Ok(())
    }

    fn reject_erased_function_value_call_cycle(
        &self,
        callee_name: &str,
        params: &[FunctionValueParam],
        arguments: &[ClosureProvenance],
        span: Span,
    ) -> KuResult<()> {
        let callee = self.get_allow_moved(callee_name, span)?;
        let captured_targets = callee
            .closure_provenance
            .dependencies
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for (param, argument) in params.iter().zip(arguments) {
            if param
                .ty
                .as_ref()
                .is_some_and(|ty| !self.type_may_contain_function_value(ty))
            {
                continue;
            }
            let mut visited = HashSet::new();
            let reaches_callee = argument.dependencies.iter().copied().any(|dependency| {
                self.closure_dependency_reaches(dependency, callee.binding_id, &mut visited)
            });
            let reaches_environment = captured_targets.iter().copied().any(|target| {
                let mut visited = HashSet::new();
                argument.dependencies.iter().copied().any(|dependency| {
                    self.closure_dependency_reaches(dependency, target, &mut visited)
                })
            });
            if reaches_callee || reaches_environment {
                return Err(KuError::runtime(
                    format!(
                        "E0904 cannot create closure reference cycle involving '{callee_name}': the function body is unavailable and may retain a back-referencing function argument"
                    ),
                    span,
                ));
            }
        }
        Ok(())
    }

    fn known_function_body_effect_summary(
        &self,
        function: ClosureBodyView<'_>,
        arguments: &[ClosureProvenance],
        argument_types: &[Type],
        captured_environment: &ClosureProvenance,
        summaries: &mut ClosureSummaryContext,
    ) -> ClosureEffectSummary {
        let ClosureBodyView {
            params,
            body,
            body_id,
        } = function;
        if params.len() != arguments.len()
            || params.len() != argument_types.len()
            || body.is_empty()
        {
            return ClosureEffectSummary {
                effects: Vec::new(),
                complete: false,
            };
        }
        // Top-level functions have no lexical capture table; they still need
        // their parameter-mediated calls analysed at each concrete call site.
        let outer_bindings = self
            .function_body_outer_bindings
            .get(&body_id)
            .cloned()
            .unwrap_or_default();
        let key = ClosureEffectSummaryKey {
            body_id,
            captured_environment: captured_environment.into(),
            arguments: arguments.iter().map(Into::into).collect(),
            argument_bodies: argument_types
                .iter()
                .map(|ty| match ty {
                    Type::FunctionValue { body_id, .. } => *body_id,
                    _ => None,
                })
                .collect(),
        };
        if let Some(cached) = summaries.effect_cache.get(&key) {
            return cached.clone();
        }
        if summaries.remaining_states == 0 || !summaries.active_effect_bodies.insert(body_id) {
            return ClosureEffectSummary {
                effects: Vec::new(),
                complete: false,
            };
        }
        summaries.remaining_states -= 1;

        let mut symbolic = HashMap::new();
        for (name, binding_id) in &outer_bindings {
            let provenance = self
                .binding_by_id(*binding_id)
                .map(|binding| binding.closure_provenance.clone())
                .unwrap_or_else(|| captured_environment.clone());
            symbolic.insert(name.clone(), provenance);
        }
        let mut locals = HashSet::new();
        let mut types = HashMap::new();
        for ((param, provenance), argument_type) in params.iter().zip(arguments).zip(argument_types)
        {
            locals.insert(param.name.clone());
            symbolic.insert(param.name.clone(), provenance.clone());
            types.insert(param.name.clone(), argument_type.clone());
        }
        let flow = self.function_closure_effect_flow(
            body,
            &outer_bindings,
            ClosureEffectEnvironment {
                symbolic,
                types,
                locals,
            },
            summaries,
        );
        summaries.active_effect_bodies.remove(&body_id);
        let summary = ClosureEffectSummary {
            effects: flow.effects,
            complete: flow.complete,
        };
        summaries.effect_cache.insert(key, summary.clone());
        summary
    }

    fn expression_call_effect_summary(
        &self,
        expr: &Expr,
        outer_bindings: &HashMap<String, BindingId>,
        environment: &ClosureEffectEnvironment,
        summaries: &mut ClosureSummaryContext,
    ) -> Option<ClosureEffectSummary> {
        let ExprKind::Call { callee, args } = &expr.kind else {
            return None;
        };
        let ExprKind::Variable(name) = &callee.kind else {
            return None;
        };
        let (params, body, body_id, captured_environment) =
            if let Some(actual_type) = environment.types.get(name) {
                let Type::FunctionValue {
                    params,
                    body,
                    body_id: Some(body_id),
                    ..
                } = actual_type
                else {
                    return None;
                };
                (
                    params.clone(),
                    body.clone(),
                    *body_id,
                    environment
                        .symbolic
                        .get(name)
                        .cloned()
                        .unwrap_or_else(ClosureProvenance::unknown),
                )
            } else if let Some(binding_id) = outer_bindings.get(name) {
                let binding = self.binding_by_id(*binding_id)?;
                let Type::FunctionValue {
                    params,
                    body,
                    body_id: Some(body_id),
                    ..
                } = &binding.ty
                else {
                    return None;
                };
                (
                    params.clone(),
                    body.clone(),
                    *body_id,
                    environment
                        .symbolic
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| binding.closure_provenance.clone()),
                )
            } else if let Some(function) = self.functions.get(name) {
                (
                    function.value_params.clone(),
                    function.body.clone(),
                    function.body_id,
                    ClosureProvenance::empty(),
                )
            } else {
                return None;
            };
        if body.is_empty() {
            return None;
        }
        let arguments = args
            .iter()
            .map(|arg| {
                self.effect_expression_closure_provenance(
                    arg,
                    outer_bindings,
                    environment,
                    summaries,
                )
            })
            .collect::<Vec<_>>();
        let argument_types = args
            .iter()
            .map(|arg| self.effect_expression_type(arg, outer_bindings, environment))
            .collect::<Vec<_>>();
        Some(self.known_function_body_effect_summary(
            ClosureBodyView {
                params: &params,
                body: &body,
                body_id,
            },
            &arguments,
            &argument_types,
            &captured_environment,
            summaries,
        ))
    }

    fn effect_expression_type(
        &self,
        expr: &Expr,
        outer_bindings: &HashMap<String, BindingId>,
        environment: &ClosureEffectEnvironment,
    ) -> Type {
        match &expr.kind {
            ExprKind::Variable(name) => {
                if let Some(ty) = environment.types.get(name) {
                    return ty.clone();
                }
                if let Some(binding_id) = outer_bindings.get(name) {
                    if let Some(binding) = self.binding_by_id(*binding_id) {
                        return binding.ty.clone();
                    }
                }
                self.functions
                    .get(name)
                    .and_then(|function| function_value_type(name, function, expr.span).ok())
                    .unwrap_or(Type::Unknown)
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { target, name } = &callee.kind {
                    if name == "clone" && args.is_empty() {
                        return self.effect_expression_type(target, outer_bindings, environment);
                    }
                }
                let callee_type = self.effect_expression_type(callee, outer_bindings, environment);
                match callee_type {
                    Type::FunctionValue { return_type, .. } => {
                        return_type.map_or(Type::Null, |ty| *ty)
                    }
                    _ => Type::Unknown,
                }
            }
            _ => Type::Unknown,
        }
    }

    fn effect_expression_closure_provenance(
        &self,
        expr: &Expr,
        outer_bindings: &HashMap<String, BindingId>,
        environment: &ClosureEffectEnvironment,
        summaries: &mut ClosureSummaryContext,
    ) -> ClosureProvenance {
        if let ExprKind::Call { callee, args } = &expr.kind {
            if let ExprKind::Field { target, name } = &callee.kind {
                if name == "clone" && args.is_empty() {
                    return self.effect_expression_closure_provenance(
                        target,
                        outer_bindings,
                        environment,
                        summaries,
                    );
                }
            }
            let arguments = args
                .iter()
                .map(|arg| {
                    self.effect_expression_closure_provenance(
                        arg,
                        outer_bindings,
                        environment,
                        summaries,
                    )
                })
                .collect::<Vec<_>>();
            if let ExprKind::Variable(name) = &callee.kind {
                if let Some(Type::FunctionValue {
                    params,
                    body,
                    body_id: Some(body_id),
                    ..
                }) = environment.types.get(name)
                {
                    if !body.is_empty() {
                        let parameter_types = params
                            .iter()
                            .map(|param| param.ty.clone().unwrap_or(Type::Unknown))
                            .collect::<Vec<_>>();
                        let captured_environment = environment
                            .symbolic
                            .get(name)
                            .cloned()
                            .unwrap_or_else(ClosureProvenance::unknown);
                        return self.known_function_body_return_provenance(
                            ClosureBodyView {
                                params,
                                body,
                                body_id: *body_id,
                            },
                            &parameter_types,
                            &arguments,
                            &captured_environment,
                            summaries,
                        );
                    }
                }
                // A parameter/local shadows a same-named top-level function. If
                // its concrete body is unavailable, retain the unknown call
                // below instead of analysing the unrelated top-level body.
                if !environment.locals.contains(name) {
                    if let Some(binding_id) = outer_bindings.get(name) {
                        if let Some(binding) = self.binding_by_id(*binding_id) {
                            if let Type::FunctionValue {
                                params,
                                body,
                                body_id: Some(body_id),
                                ..
                            } = &binding.ty
                            {
                                if !body.is_empty() {
                                    let parameter_types = params
                                        .iter()
                                        .map(|param| param.ty.clone().unwrap_or(Type::Unknown))
                                        .collect::<Vec<_>>();
                                    let captured_environment = environment
                                        .symbolic
                                        .get(name)
                                        .unwrap_or(&binding.closure_provenance);
                                    return self.known_function_body_return_provenance(
                                        ClosureBodyView {
                                            params,
                                            body,
                                            body_id: *body_id,
                                        },
                                        &parameter_types,
                                        &arguments,
                                        captured_environment,
                                        summaries,
                                    );
                                }
                            }
                        }
                    }
                }
                if environment.locals.contains(name) || outer_bindings.contains_key(name) {
                    let mut unknown = environment
                        .symbolic
                        .get(name)
                        .cloned()
                        .unwrap_or_else(ClosureProvenance::unknown);
                    for argument in &arguments {
                        unknown.merge(argument);
                    }
                    unknown.complete = false;
                    return unknown;
                }
                if let Some(function) = self.functions.get(name) {
                    return self.known_function_body_return_provenance(
                        ClosureBodyView {
                            params: &function.value_params,
                            body: &function.body,
                            body_id: function.body_id,
                        },
                        &function.params,
                        &arguments,
                        &ClosureProvenance::empty(),
                        summaries,
                    );
                }
            }
            let mut unknown = self.expression_closure_provenance_inner(
                callee,
                &environment.symbolic,
                summaries,
                false,
            );
            for argument in &arguments {
                unknown.merge(argument);
            }
            unknown.complete = false;
            return unknown;
        }
        let mut provenance =
            self.expression_closure_provenance_inner(expr, &environment.symbolic, summaries, false);
        collect_effect_expression_outer_capture_ids(expr, outer_bindings, &mut provenance);
        provenance
    }

    fn extend_expression_closure_effects(
        &self,
        expr: &Expr,
        outer_bindings: &HashMap<String, BindingId>,
        environment: &ClosureEffectEnvironment,
        effects: &mut Vec<ClosureWriteEffect>,
        complete: &mut bool,
        summaries: &mut ClosureSummaryContext,
    ) {
        if !expr_may_call_function(expr) {
            return;
        }
        if let Some(summary) =
            self.expression_call_effect_summary(expr, outer_bindings, environment, summaries)
        {
            effects.extend(summary.effects);
            *complete &= summary.complete;
        } else {
            // A nested or dynamically selected call is not proven effect-free.
            // The bounded caller fallback will retain its callable arguments.
            *complete = false;
        }
    }

    fn function_closure_effect_flow(
        &self,
        body: &[Stmt],
        outer_bindings: &HashMap<String, BindingId>,
        mut environment: ClosureEffectEnvironment,
        summaries: &mut ClosureSummaryContext,
    ) -> ClosureEffectFlow {
        let mut effects = Vec::new();
        let mut complete = true;
        let mut falls_through = true;
        for stmt in body {
            if !falls_through {
                break;
            }
            match stmt {
                Stmt::VarDecl { name, value, .. } => {
                    self.extend_expression_closure_effects(
                        value,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                    let actual_type =
                        self.effect_expression_type(value, outer_bindings, &environment);
                    let provenance = self.effect_expression_closure_provenance(
                        value,
                        outer_bindings,
                        &environment,
                        summaries,
                    );
                    environment.locals.insert(name.clone());
                    environment.symbolic.insert(name.clone(), provenance);
                    environment.types.insert(name.clone(), actual_type);
                }
                Stmt::Assign { name, value, .. } => {
                    self.extend_expression_closure_effects(
                        value,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                    let actual_type =
                        self.effect_expression_type(value, outer_bindings, &environment);
                    let provenance = self.effect_expression_closure_provenance(
                        value,
                        outer_bindings,
                        &environment,
                        summaries,
                    );
                    if environment.locals.contains(name) {
                        environment.symbolic.insert(name.clone(), provenance);
                        environment.types.insert(name.clone(), actual_type);
                    } else if let Some(target) = outer_bindings.get(name) {
                        effects.push(ClosureWriteEffect {
                            target: *target,
                            provenance: provenance.clone(),
                        });
                        environment.symbolic.insert(name.clone(), provenance);
                        environment.types.insert(name.clone(), actual_type);
                    } else {
                        environment.locals.insert(name.clone());
                        environment.symbolic.insert(name.clone(), provenance);
                        environment.types.insert(name.clone(), actual_type);
                    }
                }
                Stmt::AssignTarget { target, value, .. } => {
                    self.extend_expression_closure_effects(
                        value,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                    let provenance = self.effect_expression_closure_provenance(
                        value,
                        outer_bindings,
                        &environment,
                        summaries,
                    );
                    if let Some(name) = assign_target_root_name(target) {
                        if !environment.locals.contains(name) {
                            if let Some(target) = outer_bindings.get(name) {
                                effects.push(ClosureWriteEffect {
                                    target: *target,
                                    provenance,
                                });
                            }
                        }
                    }
                }
                Stmt::DestructureAssign { names, values, .. } => {
                    for (name, value) in names.iter().zip(values) {
                        let Some(name) = name else {
                            continue;
                        };
                        self.extend_expression_closure_effects(
                            value,
                            outer_bindings,
                            &environment,
                            &mut effects,
                            &mut complete,
                            summaries,
                        );
                        let provenance = self.effect_expression_closure_provenance(
                            value,
                            outer_bindings,
                            &environment,
                            summaries,
                        );
                        let actual_type =
                            self.effect_expression_type(value, outer_bindings, &environment);
                        if environment.locals.contains(name) {
                            environment.symbolic.insert(name.clone(), provenance);
                            environment.types.insert(name.clone(), actual_type);
                        } else if let Some(target) = outer_bindings.get(name) {
                            effects.push(ClosureWriteEffect {
                                target: *target,
                                provenance: provenance.clone(),
                            });
                            environment.symbolic.insert(name.clone(), provenance);
                            environment.types.insert(name.clone(), actual_type);
                        } else {
                            environment.locals.insert(name.clone());
                            environment.symbolic.insert(name.clone(), provenance);
                            environment.types.insert(name.clone(), actual_type);
                        }
                    }
                }
                Stmt::ObjectDestructureAssign {
                    bindings,
                    rest,
                    value,
                    ..
                } => {
                    self.extend_expression_closure_effects(
                        value,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                    let provenance = self.effect_expression_closure_provenance(
                        value,
                        outer_bindings,
                        &environment,
                        summaries,
                    );
                    let mut names = bindings
                        .iter()
                        .filter_map(|binding| binding.local.as_ref())
                        .collect::<Vec<_>>();
                    names.extend(rest.iter().filter_map(|rest| rest.local.as_ref()));
                    for name in names {
                        if environment.locals.contains(name) {
                            environment
                                .symbolic
                                .insert(name.clone(), provenance.clone());
                        } else if let Some(target) = outer_bindings.get(name) {
                            effects.push(ClosureWriteEffect {
                                target: *target,
                                provenance: provenance.clone(),
                            });
                            environment
                                .symbolic
                                .insert(name.clone(), provenance.clone());
                        } else {
                            environment.locals.insert(name.clone());
                            environment
                                .symbolic
                                .insert(name.clone(), provenance.clone());
                        }
                    }
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    self.extend_expression_closure_effects(
                        condition,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                    let then_flow = self.function_closure_effect_flow(
                        then_branch,
                        outer_bindings,
                        environment.clone(),
                        summaries,
                    );
                    let else_flow = self.function_closure_effect_flow(
                        else_branch,
                        outer_bindings,
                        environment.clone(),
                        summaries,
                    );
                    let reachable = [&then_flow, &else_flow]
                        .into_iter()
                        .filter(|flow| flow.falls_through)
                        .map(|flow| flow.environment.clone())
                        .collect::<Vec<_>>();
                    if reachable.is_empty() {
                        falls_through = false;
                    } else {
                        for (name, current) in environment.symbolic.iter_mut() {
                            let mut merged = ClosureProvenance::empty();
                            for branch in &reachable {
                                merged.merge(branch.symbolic.get(name).unwrap_or(current));
                            }
                            *current = merged;
                        }
                        for (name, current) in environment.types.iter_mut() {
                            let mut branch_types = reachable
                                .iter()
                                .map(|branch| {
                                    branch.types.get(name).cloned().unwrap_or(Type::Unknown)
                                })
                                .collect::<Vec<_>>();
                            let first = branch_types.pop().unwrap_or(Type::Unknown);
                            *current = if branch_types.iter().all(|ty| ty == &first) {
                                first
                            } else {
                                // A later call must not reuse one branch's body
                                // summary for another branch's FunctionValue.
                                Type::Unknown
                            };
                        }
                    }
                    complete &= then_flow.complete && else_flow.complete;
                    effects.extend(then_flow.effects);
                    effects.extend(else_flow.effects);
                }
                Stmt::While { body, .. } | Stmt::For { body, .. } => {
                    let loop_flow = self.function_closure_effect_flow(
                        body,
                        outer_bindings,
                        environment.clone(),
                        summaries,
                    );
                    effects.extend(loop_flow.effects);
                    for (name, current) in environment.symbolic.iter_mut() {
                        if let Some(loop_value) = loop_flow.environment.symbolic.get(name) {
                            current.merge(loop_value);
                        }
                    }
                    for (name, current) in environment.types.iter_mut() {
                        if loop_flow.environment.types.get(name) != Some(current) {
                            *current = Type::Unknown;
                        }
                    }
                    complete = false;
                }
                Stmt::Try {
                    body,
                    catch_body,
                    finally_body,
                    ..
                } => {
                    for block in [
                        body.as_slice(),
                        catch_body.as_slice(),
                        finally_body.as_slice(),
                    ] {
                        let flow = self.function_closure_effect_flow(
                            block,
                            outer_bindings,
                            environment.clone(),
                            summaries,
                        );
                        effects.extend(flow.effects);
                    }
                    complete = false;
                }
                Stmt::Function(function) => {
                    let mut provenance = ClosureProvenance::empty();
                    for name in crate::runtime::interpreter::function_capture_names(function) {
                        if let Some(captured) = environment.symbolic.get(&name) {
                            provenance.merge(captured);
                        }
                    }
                    environment.locals.insert(function.name.clone());
                    environment
                        .symbolic
                        .insert(function.name.clone(), provenance);
                }
                Stmt::Expr { expr, .. } | Stmt::Print { value: expr, .. } => {
                    self.extend_expression_closure_effects(
                        expr,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                }
                Stmt::Return { value, .. } => {
                    if let Some(value) = value {
                        self.extend_expression_closure_effects(
                            value,
                            outer_bindings,
                            &environment,
                            &mut effects,
                            &mut complete,
                            summaries,
                        );
                    }
                    falls_through = false;
                }
                Stmt::Fail { value, .. } | Stmt::Panic { value, .. } => {
                    self.extend_expression_closure_effects(
                        value,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                    falls_through = false;
                }
                Stmt::Break { .. } | Stmt::Continue { .. } => falls_through = false,
                Stmt::CompoundAssign { value, .. } => {
                    self.extend_expression_closure_effects(
                        value,
                        outer_bindings,
                        &environment,
                        &mut effects,
                        &mut complete,
                        summaries,
                    );
                }
            }
        }
        ClosureEffectFlow {
            environment,
            effects,
            complete,
            falls_through,
        }
    }

    fn binding_name_and_type_by_id(&self, binding_id: BindingId) -> Option<(String, Type)> {
        self.scopes.iter().rev().find_map(|scope| {
            scope.iter().find_map(|(name, binding)| {
                (binding.binding_id == binding_id).then(|| (name.clone(), binding.ty.clone()))
            })
        })
    }

    fn type_may_contain_function_value(&self, ty: &Type) -> bool {
        self.type_may_contain_function_value_inner(ty, &mut HashSet::new(), &mut HashSet::new())
    }

    fn type_may_contain_function_value_inner(
        &self,
        ty: &Type,
        visiting_structs: &mut HashSet<String>,
        visiting_enums: &mut HashSet<String>,
    ) -> bool {
        match ty {
            Type::FunctionValue { .. }
            | Type::DynamicObject
            | Type::KuValue
            | Type::Generic(_)
            | Type::Unknown => true,
            Type::Array(inner) | Type::Result(inner) | Type::Task(inner) => {
                self.type_may_contain_function_value_inner(inner, visiting_structs, visiting_enums)
            }
            Type::Union(types) => types.iter().any(|ty| {
                self.type_may_contain_function_value_inner(ty, visiting_structs, visiting_enums)
            }),
            Type::Object(fields) => fields.values().any(|ty| {
                self.type_may_contain_function_value_inner(ty, visiting_structs, visiting_enums)
            }),
            Type::Struct(name) => {
                if !visiting_structs.insert(name.clone()) {
                    return false;
                }
                let contains = self.structs.get(name).is_some_and(|layout| {
                    layout.fields.values().any(|field| {
                        self.type_may_contain_function_value_inner(
                            field,
                            visiting_structs,
                            visiting_enums,
                        )
                    })
                });
                visiting_structs.remove(name);
                contains
            }
            Type::Enum(name) => {
                if !visiting_enums.insert(name.clone()) {
                    return false;
                }
                let contains = self.enums.get(name).is_some_and(|layout| {
                    layout.variants.values().flatten().any(|field| {
                        self.type_may_contain_function_value_inner(
                            field,
                            visiting_structs,
                            visiting_enums,
                        )
                    })
                });
                visiting_enums.remove(name);
                contains
            }
            Type::Int
            | Type::Float
            | Type::Bool
            | Type::String
            | Type::Null
            | Type::StringMap
            | Type::Native(_)
            | Type::Void => false,
        }
    }

    fn mark_initialized(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(var) = scope.get_mut(name) {
                // Reassigning the whole variable re-initializes every path.
                var.reinit(&[]);
                return;
            }
        }
    }

    /// The type of a resolved movable place: the root variable's type projected
    /// through its static field path. `None` if the path leaves struct territory
    /// (which never happens for a place built by `classify_place`).
    /// True when `root` names a `catch` error binding (its struct-backed fields
    /// are movable, unlike a same-shaped user object literal).
    fn is_struct_backed_binding(&self, root: &str) -> bool {
        self.get_allow_moved(root, Span::default())
            .map(|var| var.struct_backed)
            .unwrap_or(false)
    }

    /// True when a field of the place `pp` is a movable struct field: `pp` is a
    /// struct value, or it is the error binding itself (whose fields are
    /// struct-backed at runtime).
    fn field_is_movable(&self, pp: &PlacePath) -> bool {
        matches!(self.place_type(pp), Some(Type::Struct(_)))
            || (pp.path.is_empty() && self.is_struct_backed_binding(&pp.root))
    }

    fn place_type(&self, place: &PlacePath) -> Option<Type> {
        let mut ty = self.get_allow_moved(&place.root, Span::default()).ok()?.ty;
        for field in &place.path {
            ty = match &ty {
                Type::Struct(name) => self.structs.get(name)?.fields.get(field)?.clone(),
                // A struct-backed object (a caught error, an HTTP request) projects
                // through its declared shape too, so `req.params` resolves to the
                // string map rather than stopping the walk.
                Type::Object(fields) => fields.get(field)?.clone(),
                _ => return None,
            };
        }
        Some(ty)
    }

    /// Classify what kind of place an expression denotes, for move analysis:
    ///   * `Movable` — a local variable or a chain of static struct fields rooted
    ///     at one. These support path-level partial move (and the C backend can
    ///     move-and-clear them).
    ///   * `Index` — an array element or an object field/index. The backend cannot
    ///     move-and-clear these, so moving an owned value here is rejected and an
    ///     explicit `.clone()` (or `take`/`remove`) is required.
    ///   * `Fresh` — a temporary that owns no other binding (a call result,
    ///     literal, constructor, or `.clone()`); moving it tracks nothing.
    fn classify_place(&self, expr: &Expr) -> PlaceClass {
        match &expr.kind {
            ExprKind::Variable(name) if self.contains(name) => PlaceClass::Movable(PlacePath {
                root: name.clone(),
                path: Vec::new(),
            }),
            ExprKind::Field { target, name } => {
                // `EnumName.Variant` is a constructor, not a projection.
                if let ExprKind::Variable(enum_name) = &target.kind {
                    if self.enums.contains_key(enum_name) {
                        return PlaceClass::Fresh;
                    }
                }
                match self.classify_place(target) {
                    PlaceClass::Movable(pp) => {
                        // A static field of a struct (or of the struct-backed catch
                        // error object) is a movable place. A field of a user object
                        // / string-map / dynamic object is a `KuObject` hash-map
                        // entry the backend cannot move-and-clear, so it is an Index
                        // place requiring `.clone()`.
                        if self.field_is_movable(&pp) {
                            let mut path = pp.path;
                            path.push(name.clone());
                            PlaceClass::Movable(PlacePath {
                                root: pp.root,
                                path,
                            })
                        } else {
                            self.container_read_class(self.place_type(&pp))
                        }
                    }
                    other => other,
                }
            }
            ExprKind::Index { target, .. } | ExprKind::OptionalField { target, .. } => {
                let container = match self.classify_place(target) {
                    PlaceClass::Movable(pp) => self.place_type(&pp),
                    _ => None,
                };
                self.container_read_class(container)
            }
            _ => PlaceClass::Fresh,
        }
    }

    /// How reading an element out of `container` behaves in the native backend.
    ///
    /// Array elements are returned by `ku_array_get_*` as a shallow copy that still
    /// aliases the container's buffer, so consuming one would double free — those
    /// are `Index` places and require an explicit `.clone()`. Object, string-map and
    /// dynamic-object lookups go through `ku_object_get_result` / `ku_http_map_get`,
    /// which `ku_value_clone` the entry, so the read already yields an independent
    /// value: consuming it moves nothing and is `Fresh`.
    fn container_read_class(&self, container: Option<Type>) -> PlaceClass {
        match container {
            Some(Type::Object(_)) | Some(Type::StringMap) | Some(Type::DynamicObject) => {
                PlaceClass::Fresh
            }
            Some(Type::String) => PlaceClass::Fresh,
            _ => PlaceClass::Index,
        }
    }

    /// The movable place an assignment target writes to, if it is a variable or a
    /// static struct-field chain. Assigning it re-initializes that path.
    fn assign_target_place(&self, target: &AssignTarget) -> Option<PlacePath> {
        match target {
            AssignTarget::Variable(name) if self.contains(name) => Some(PlacePath {
                root: name.clone(),
                path: Vec::new(),
            }),
            AssignTarget::Field { target, name } => match self.classify_place(target) {
                PlaceClass::Movable(pp) => self.field_is_movable(&pp).then(|| {
                    let mut path = pp.path;
                    path.push(name.clone());
                    PlacePath {
                        root: pp.root,
                        path,
                    }
                }),
                _ => None,
            },
            _ => None,
        }
    }

    /// Re-initialize the place `place` after an assignment to it (`user.name = x`
    /// makes `user.name` — and anything under it — live again).
    fn reinit_place(&mut self, place: &PlacePath) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(var) = scope.get_mut(&place.root) {
                var.reinit(&place.path);
                return;
            }
        }
    }

    /// Reject reading `place` unless it is FULLY live: neither the place, nor an
    /// ancestor, nor any descendant of it was moved. Used where the whole value is
    /// consumed as a unit (e.g. `.clone()`), which a partial move invalidates.
    fn check_place_fully_live(&self, place: &PlacePath, span: Span) -> KuResult<()> {
        if let Ok(var) = self.get_allow_moved(&place.root, span) {
            if let Some(mark) = var.read_block(&place.path) {
                return Err(read_of_moved_error(&place.root, &place.path, mark, span));
            }
        }
        Ok(())
    }

    /// Reject reading `place` when the exact path or an ancestor of it was moved
    /// out (its value is gone). A moved descendant does not block — reading a
    /// sibling field, or resolving an intermediate projection base, stays legal.
    fn check_place_readable(&self, place: &PlacePath, span: Span) -> KuResult<()> {
        let Ok(var) = self.get_allow_moved(&place.root, span) else {
            return Ok(());
        };
        for (q, mark) in &var.moves {
            if path_is_prefix(q, &place.path) {
                return Err(read_of_moved_error(&place.root, &place.path, *mark, span));
            }
        }
        Ok(())
    }

    /// Record that the movable place `place` has been moved out. Enforces the
    /// closure capture boundary (E0904) on the root and that the place is still
    /// live (a double move / move-through-moved-parent is rejected).
    fn record_move(&mut self, place: &PlacePath, span: Span) -> KuResult<()> {
        self.reject_readonly_capture_move(place, span)?;
        let boundary = self.closure_capture_boundaries.last().copied();
        for (index, scope) in self.scopes.iter_mut().enumerate().rev() {
            if let Some(var) = scope.get_mut(&place.root) {
                if let Some(boundary) = boundary {
                    if index < boundary {
                        return Err(KuError::runtime(
                            format!(
                                "cannot move captured owned value '{}' out of a closure; use '{}.clone()'",
                                place.root, place.root
                            ),
                            span,
                        ));
                    }
                }
                if var.captured {
                    return Err(KuError::runtime(
                        format!(
                            "cannot move '{}': a closure captured it, and the closure reads the same value; use '{}.clone()'",
                            place.root, place.root
                        ),
                        span,
                    ));
                }
                if var.read_block(&place.path).is_some() {
                    return Err(move_of_moved_error(&place.root, &place.path, span));
                }
                var.mark_moved(place.path.clone(), MoveMark::Moved);
                return Ok(());
            }
        }
        Ok(())
    }

    fn consume_expr(&mut self, expr: &Expr) -> KuResult<Type> {
        let ty = self.check_expr(expr)?;
        if !self.is_owned_type(&ty) {
            // Copy types (int/float/bool/null) and other non-owned reads never
            // move, whatever place they come from.
            return Ok(ty);
        }
        match self.classify_place(expr) {
            PlaceClass::Movable(place) => self.record_move(&place, expr.span)?,
            PlaceClass::Index => return Err(index_move_error(expr.span)),
            PlaceClass::Fresh => {}
        }
        Ok(ty)
    }

    /// The declared type of `obj["key"]` when both the object's shape and the key
    /// are known statically. `Type::DynamicObject` and computed keys stay unknown.
    fn static_index_element_type(&mut self, expr: &Expr) -> Option<Type> {
        let ExprKind::Index { target, index } = &expr.kind else {
            return None;
        };
        let ExprKind::Literal(Literal::String(key)) = &index.kind else {
            return None;
        };
        let Type::Object(fields) = self.check_expr(target).ok()? else {
            return None;
        };
        fields.get(key.as_str()).cloned()
    }

    fn consume_await_task_expr(&mut self, expr: &Expr, await_span: Span) -> KuResult<Type> {
        if let ExprKind::Variable(name) = &expr.kind {
            let var = self.get_allow_moved(name, expr.span)?;
            if var.whole_move().is_some() && matches!(var.ty, Type::Task(_)) {
                return Err(KuError::runtime(
                    format!("task '{name}' has already been awaited"),
                    await_span,
                ));
            }
        }
        self.consume_expr(expr)
    }

    fn is_owned_type(&self, ty: &Type) -> bool {
        match ty {
            Type::String
            | Type::Array(_)
            | Type::Object(_)
            | Type::StringMap
            | Type::DynamicObject
            | Type::KuValue
            // A native handle owns a C resource; it must be move-tracked and dropped.
            | Type::Native(_) => true,
            Type::Struct(name) => {
                self.structs.get(name).is_some_and(|layout| {
                    layout
                        .fields
                        .values()
                        .any(|field| self.is_owned_type(field))
                }) || self.structs.contains_key(name)
            }
            Type::Enum(name) => {
                self.enums.get(name).is_some_and(|layout| {
                    layout
                        .variants
                        .values()
                        .flatten()
                        .any(|field| self.is_owned_type(field))
                }) || self.enums.contains_key(name)
            }
            Type::Result(_) | Type::Task(_) => true,
            // Stage 6d: a function value owns its captured environment (a
            // ref-counted cell chain), so it is an owned type: `.clone()` bumps
            // the env refcount, and storing one into a binding/field/array/return
            // moves it. Calling or passing it as an argument only borrows (see
            // `consume_arg_expr_expecting`).
            Type::FunctionValue { .. } => true,
            Type::Union(types) => types.iter().any(|ty| self.is_owned_type(ty)),
            _ => false,
        }
    }

    /// Native handles are move-only even when nested inside another owned value.
    /// Walk named layouts recursively, but break cycles so an invalid/self-recursive
    /// user type cannot overflow the checker while clone eligibility is decided.
    fn type_contains_native_resource(&self, ty: &Type) -> bool {
        self.type_contains_native_resource_inner(ty, &mut HashSet::new(), &mut HashSet::new())
    }

    fn type_contains_native_resource_inner(
        &self,
        ty: &Type,
        visiting_structs: &mut HashSet<String>,
        visiting_enums: &mut HashSet<String>,
    ) -> bool {
        match ty {
            // `bytes` is an ordinary cloneable owned value. Other native types
            // carry external resource identity and remain move-only, including
            // when nested inside arrays/results/user aggregates.
            Type::Native(name) => name != metadata::BYTES,
            Type::Array(inner) | Type::Result(inner) | Type::Task(inner) => {
                self.type_contains_native_resource_inner(inner, visiting_structs, visiting_enums)
            }
            Type::Union(types) => types.iter().any(|ty| {
                self.type_contains_native_resource_inner(ty, visiting_structs, visiting_enums)
            }),
            Type::Object(fields) => fields.values().any(|ty| {
                self.type_contains_native_resource_inner(ty, visiting_structs, visiting_enums)
            }),
            Type::Struct(name) => {
                if !visiting_structs.insert(name.clone()) {
                    return false;
                }
                let contains = self.structs.get(name).is_some_and(|layout| {
                    layout.fields.values().any(|ty| {
                        self.type_contains_native_resource_inner(
                            ty,
                            visiting_structs,
                            visiting_enums,
                        )
                    })
                });
                visiting_structs.remove(name);
                contains
            }
            Type::Enum(name) => {
                if !visiting_enums.insert(name.clone()) {
                    return false;
                }
                let contains = self.enums.get(name).is_some_and(|layout| {
                    layout.variants.values().flatten().any(|ty| {
                        self.type_contains_native_resource_inner(
                            ty,
                            visiting_structs,
                            visiting_enums,
                        )
                    })
                });
                visiting_enums.remove(name);
                contains
            }
            Type::Int
            | Type::Float
            | Type::Bool
            | Type::String
            | Type::Null
            | Type::StringMap
            | Type::DynamicObject
            | Type::Generic(_)
            | Type::Void
            | Type::FunctionValue { .. }
            | Type::KuValue
            | Type::Unknown => false,
        }
    }

    /// The move state after a loop, joining every way it can exit: running zero
    /// times (`before`), falling through / the condition going false (`end`), and
    /// each `break` point. A value moved on any reachable exit is (maybe) moved
    /// after the loop.
    fn after_loop_state(
        &self,
        before: Vec<HashMap<String, VarType>>,
        end: Vec<HashMap<String, VarType>>,
        breaks: Vec<Vec<HashMap<String, VarType>>>,
    ) -> Vec<HashMap<String, VarType>> {
        let mut exits = vec![before.clone(), end];
        exits.extend(breaks);
        merge_moved_scope_paths(before, &exits)
    }

    /// The move state at the top of a loop iteration. A loop with a back-edge can
    /// carry a move from one iteration into the next, so any owned value moved in
    /// the body is `MaybeMoved` at the top; the authoritative pass then rejects a
    /// use before re-initialization (a loop-carried move) but accepts a value that
    /// the body re-initializes at the top before using it. A loop that cannot
    /// iterate (no back-edge) keeps the pre-loop state.
    ///
    /// This runs a throwaway pass over the body to discover its moves; its scope
    /// mutations are rolled back and its errors ignored (the authoritative pass
    /// re-checks the body and surfaces any real error).
    fn compute_loop_top(
        &mut self,
        before: &[HashMap<String, VarType>],
        body: &[Stmt],
        loop_var: Option<(&str, &Type, &ClosureProvenance)>,
    ) -> Vec<HashMap<String, VarType>> {
        if !loop_body_has_backedge(body) {
            return before.to_vec();
        }
        let saved_scopes = self.scopes.clone();
        // This is a speculative ownership pass. Abrupt exits are recorded by the
        // authoritative pass below, not by this throwaway scan.
        let saved_try_exit_collectors = std::mem::take(&mut self.try_exit_collectors);
        let saved_next_binding_id = self.next_binding_id;
        let saved_next_function_body_id = self.next_function_body_id;
        let saved_body_bindings = self.function_body_outer_bindings.clone();
        let outer_binding_count = before.iter().map(HashMap::len).sum::<usize>();
        // Each iteration can add at least one edge along a simple BindingId path;
        // N+2 passes therefore cover an N-node chain. The hard cap prevents an
        // adversarial body from multiplying full checker passes without bound.
        let max_iterations = outer_binding_count.saturating_add(2).clamp(2, 128);
        let mut top = before.to_vec();
        let mut converged = false;
        for _ in 0..max_iterations {
            // Speculative locals and closure bodies must receive the same ids on
            // every pass; otherwise Type/body-id churn would prevent convergence.
            self.next_binding_id = saved_next_binding_id;
            self.next_function_body_id = saved_next_function_body_id;
            self.function_body_outer_bindings = saved_body_bindings.clone();
            let candidate = self.speculative_loop_transfer(before, &top, body, loop_var);
            if candidate == top {
                top = candidate;
                converged = true;
                break;
            }
            top = candidate;
        }
        if !converged {
            // Fail closed after the explicit budget: every function-capable loop
            // binding may reach every other one. The authoritative pass then
            // reports E0904 at the first write that could close such a cycle.
            let possible_targets = top
                .iter()
                .flat_map(|scope| scope.values())
                .filter(|binding| self.type_may_contain_function_value(&binding.ty))
                .map(|binding| binding.binding_id)
                .collect::<HashSet<_>>();
            for binding in top.iter_mut().flat_map(|scope| scope.values_mut()) {
                if self.type_may_contain_function_value(&binding.ty) {
                    binding
                        .closure_provenance
                        .dependencies
                        .extend(possible_targets.iter().copied());
                    binding.closure_provenance.complete = false;
                }
            }
        }
        self.scopes = saved_scopes;
        self.try_exit_collectors = saved_try_exit_collectors;
        top
    }

    fn speculative_loop_transfer(
        &mut self,
        before: &[HashMap<String, VarType>],
        iteration_top: &[HashMap<String, VarType>],
        body: &[Stmt],
        loop_var: Option<(&str, &Type, &ClosureProvenance)>,
    ) -> Vec<HashMap<String, VarType>> {
        self.scopes = iteration_top.to_vec();
        self.push_scope();
        if let Some((name, ty, provenance)) = loop_var {
            let _ = self.define(name.to_string(), ty.clone(), true, Span::default());
            let _ = self.set_closure_provenance(name, provenance.clone(), Span::default());
        }
        // The scan must run inside a loop context: without it `break`/`continue`
        // fail as "outside loop", hiding ownership changes after those exits.
        self.loop_depth += 1;
        self.loop_break_states.push(Vec::new());
        self.loop_continue_states.push(Vec::new());
        for stmt in body {
            // Errors are surfaced by the authoritative pass. Continue scanning so
            // an earlier speculative error cannot hide later graph edges.
            let _ = self.check_stmt(stmt);
            if stmt_stops_fallthrough(stmt) {
                break;
            }
        }
        self.loop_break_states.pop();
        let continues = self.loop_continue_states.pop().unwrap_or_default();
        self.loop_depth -= 1;
        self.pop_scope();
        let end_of_body = self.scopes.clone();
        let mut top = merge_moved_scopes(before.to_vec(), before.to_vec(), end_of_body);
        for state in continues {
            top = merge_moved_scopes(before.to_vec(), top, state);
        }
        top
    }

    #[allow(dead_code)]
    fn reject_loop_carried_moves(
        &self,
        before: &[HashMap<String, VarType>],
        span: Span,
    ) -> KuResult<()> {
        for (before_scope, after_scope) in before.iter().zip(&self.scopes) {
            for (name, before_var) in before_scope {
                let Some(after_var) = after_scope.get(name) else {
                    continue;
                };
                if !before_var.any_moved()
                    && after_var.any_moved()
                    && self.is_owned_type(&before_var.ty)
                {
                    return Err(KuError::runtime(
                        format!(
                            "cannot move outer owned value '{name}' from a loop body because a later iteration would reuse a moved value; use '{name}.clone()' or reinitialize it on every path"
                        ),
                        span,
                    ));
                }
            }
        }
        Ok(())
    }

    fn readonly_capture_for_outer_binding(&self, name: &str) -> Option<ReadonlyCapture> {
        let capture = self.readonly_capture?;
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, scope)| scope.contains_key(name).then_some(index))
            .filter(|index| *index < capture.boundary)
            .map(|_| capture)
    }

    fn reject_readonly_capture_assignment(&self, name: &str, span: Span) -> KuResult<()> {
        if let Some(capture) = self.readonly_capture_for_outer_binding(name) {
            return Err(KuError::runtime(
                format!("{} cannot modify captured variable '{name}'", capture.owner),
                span,
            ));
        }
        Ok(())
    }

    fn reject_readonly_capture_move(&self, place: &PlacePath, span: Span) -> KuResult<()> {
        if let Some(capture) = self.readonly_capture_for_outer_binding(&place.root) {
            return Err(KuError::runtime(
                format!(
                    "{} cannot move captured owned value '{}'",
                    capture.owner,
                    place_display(&place.root, &place.path)
                ),
                span,
            ));
        }
        Ok(())
    }

    fn reject_readonly_http_native_capture_read(&self, expr: &Expr, ty: &Type) -> KuResult<()> {
        let Type::Native(native) = ty else {
            return Ok(());
        };
        if native != metadata::PG_RESULT && native != metadata::MYSQL_RESULT {
            return Ok(());
        }
        let Some(root) = expr_root_variable(expr) else {
            return Ok(());
        };
        let Some(capture) = self.readonly_capture_for_outer_binding(root) else {
            return Ok(());
        };
        if capture.owner != "http handler" {
            return Ok(());
        }
        Err(KuError::runtime(
            format!(
                "http handler cannot share captured native resource '{}' across concurrent workers; keep only a pooled client outside the handler and create each result inside the handler",
                type_name(ty)
            ),
            expr.span,
        ))
    }

    /// HTTP control calls are not assignments, but route handlers execute later
    /// on concurrent workers. Reject them throughout the handler's reachable call
    /// tree: captured controls race the live server, while per-request controls
    /// can leak servers/sockets or block a worker.
    fn reject_http_handler_control_call(
        &self,
        target: &Expr,
        target_type: &Type,
        method: &str,
        span: Span,
    ) -> KuResult<()> {
        let resource = if is_http_service_type(target_type) {
            "service"
        } else if is_http_listener_type(target_type) {
            "listener"
        } else {
            return Ok(());
        };
        let Some(capture) = self.readonly_capture else {
            return Ok(());
        };
        if capture.owner != "http handler" {
            return Ok(());
        }
        if let Some(root) = expr_root_variable(target) {
            if self.readonly_capture_for_outer_binding(root).is_some() {
                return Err(KuError::runtime(
                    format!(
                        "http handler cannot call '{method}' on captured http {resource} rooted at '{root}'; handlers cannot modify, start, run, or close captured services/listeners"
                    ),
                    span,
                ));
            }
        }
        Err(KuError::runtime(
            format!(
                "http handler cannot call '{method}' on http {resource}; HTTP control-plane calls are forbidden in handlers because they can mutate server lifecycle, leak per-request resources, or block a worker"
            ),
            span,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
struct ReadonlyCapture {
    boundary: usize,
    owner: &'static str,
}

fn expr_root_variable(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name),
        ExprKind::Field { target, .. }
        | ExprKind::OptionalField { target, .. }
        | ExprKind::Index { target, .. } => expr_root_variable(target),
        _ => None,
    }
}

fn expect_arg_count(name: &str, actual: usize, expected: usize, span: Span) -> KuResult<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(KuError::runtime(
            format!("function '{name}' expects {expected} arguments but got {actual}"),
            span,
        ))
    }
}

fn reject_duplicate_function_value_params(params: &[FunctionParam]) -> KuResult<()> {
    let mut seen = HashSet::new();
    for param in params {
        if !seen.insert(&param.name) {
            return Err(KuError::runtime(
                format!("duplicate function value parameter '{}'", param.name),
                param.span,
            ));
        }
    }
    Ok(())
}

fn reject_duplicate_params(function: &FnDecl) -> KuResult<()> {
    let mut seen = HashSet::new();
    for param in &function.params {
        if !seen.insert(&param.name) {
            return Err(KuError::runtime(
                format!("duplicate function parameter '{}'", param.name),
                param.span,
            ));
        }
    }
    Ok(())
}

fn is_numeric(ty: &Type) -> bool {
    match ty {
        Type::Int | Type::Float => true,
        Type::Union(types) => !types.is_empty() && types.iter().all(is_numeric),
        _ => false,
    }
}

fn numeric_result(op: BinaryOp, left: &Type, right: &Type, span: Span) -> KuResult<Type> {
    if !is_numeric(left) || !is_numeric(right) {
        return Err(KuError::runtime(
            format!(
                "type error: expected numbers but got {} and {}",
                type_name(left),
                type_name(right)
            ),
            span,
        ));
    }
    if op == BinaryOp::Remainder && (left != &Type::Int || right != &Type::Int) {
        return Err(KuError::runtime(
            "type error: '%' expects int operands",
            span,
        ));
    }
    if contains_float(left) || contains_float(right) {
        Ok(Type::Float)
    } else {
        Ok(Type::Int)
    }
}

fn contains_float(ty: &Type) -> bool {
    match ty {
        Type::Float => true,
        Type::Union(types) => types.iter().any(contains_float),
        _ => false,
    }
}

fn type_matches(expected: &Type, actual: &Type) -> bool {
    match (expected, actual) {
        (Type::Generic(_), _) | (_, Type::Generic(_)) => true,
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Union(expected_options), Type::Union(actual_options)) => {
            actual_options.iter().all(|actual| {
                expected_options
                    .iter()
                    .any(|expected| type_matches(expected, actual))
            })
        }
        (Type::Union(options), _) => options.iter().any(|option| type_matches(option, actual)),
        (_, Type::Union(options)) => options.iter().all(|option| type_matches(expected, option)),
        (Type::Array(left), Type::Array(right)) => type_matches(left, right),
        (Type::Result(left), Type::Result(right)) => type_matches(left, right),
        (
            Type::FunctionValue {
                params: left_params,
                return_type: left_return,
                is_async: left_async,
                ..
            },
            Type::FunctionValue {
                params: right_params,
                return_type: right_return,
                is_async: right_async,
                ..
            },
        ) => {
            left_async == right_async
                && left_params.len() == right_params.len()
                && left_params
                    .iter()
                    .zip(right_params.iter())
                    .all(|(left, right)| {
                        function_param_matches(left.ty.as_ref(), right.ty.as_ref())
                    })
                && function_return_matches(left_return.as_deref(), right_return.as_deref())
        }
        (Type::Object(_), Type::StringMap) | (Type::StringMap, Type::Object(_)) => true,
        (Type::Object(_), Type::DynamicObject) | (Type::DynamicObject, Type::Object(_)) => true,
        (Type::Object(left), Type::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(name, left_ty)| {
                    right
                        .get(name)
                        .is_some_and(|right_ty| type_matches(left_ty, right_ty))
                })
        }
        _ => expected == actual,
    }
}

fn function_param_matches(expected: Option<&Type>, actual: Option<&Type>) -> bool {
    match (expected, actual) {
        (Some(expected), Some(actual)) => type_matches(expected, actual),
        (Some(_), None) => false,
        (None, Some(_)) | (None, None) => true,
    }
}

fn function_return_matches(expected: Option<&Type>, actual: Option<&Type>) -> bool {
    match (expected, actual) {
        (Some(expected), Some(actual)) => type_matches(expected, actual),
        (Some(_), None) => false,
        (None, Some(_)) | (None, None) => true,
    }
}

fn union_or_single(types: Vec<Type>) -> Type {
    let mut deduped = Vec::new();
    for ty in types {
        if !deduped
            .iter()
            .any(|existing| type_matches(existing, &ty) && type_matches(&ty, existing))
        {
            deduped.push(ty);
        }
    }
    if deduped.len() == 1 {
        deduped.remove(0)
    } else {
        Type::Union(deduped)
    }
}

/// Identifiers starting with `__ku_` belong to the native C backend, which emits
/// helpers and block-scoped temporaries under that prefix. A user binding of the
/// same name is silently shadowed in the generated C — `__ku_p` printed an empty
/// string and `__ku_store` crashed — so the checker reserves the prefix outright.
/// Argument positions where a stdlib/builtin call takes ownership of its argument,
/// mirroring the `c_value_expr` (move-and-clear) sites in the native C backend. The
/// checker must record a move at exactly these positions: recording none let the
/// backend move a value the checker still believed live (silent emptying), and let
/// an array/object element be moved out by indexing (aliasing double free).
///
/// Everything absent from this table borrows — `println(x)`, `len(x)`, `x.trim()`,
/// and every read-only receiver keep their argument usable.
fn stdlib_consuming_args(name: &str) -> &'static [usize] {
    match name {
        // The value is wrapped into the Result and handed to the caller.
        "ok" | "err" => &[0],
        "json.stringify" => &[0],
        "kuvalue.as_int" | "kuvalue.as_str" => &[0],
        // Closing consumes (and frees) the client; receiver reads borrow their handle.
        "pg_client.close" | "redis.close" | "mysql.close" | "net.close" => &[0],
        _ => &[],
    }
}

/// An owned element cannot be moved out of an array by indexing: `ku_array_get_*`
/// returns a shallow copy that still aliases the container's buffer, and there is
/// no way to clear the slot it came from.
///
/// The remedy is `.clone()`. Ku has no `array.take(index)` yet, so the message must
/// not advertise one — a user following that advice hits "no such method".
fn index_move_error(span: Span) -> KuError {
    KuError::runtime(
        "cannot move an owned value out of an array by indexing; add '.clone()' for an independent copy"
            .to_string(),
        span,
    )
}

fn reject_reserved_name(name: &str, span: Span) -> KuResult<()> {
    // Import expansion renames a module's items into the reserved namespace after
    // parsing, so those synthesized names reach the checker too. They cannot collide
    // with backend temporaries, and a user who writes one anyway hits the loud
    // "top-level name is already defined" error.
    if EXPANDER_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Ok(());
    }
    if name.starts_with(RESERVED_NAME_PREFIX) {
        return Err(KuError::runtime(
            format!(
                "name '{name}' is reserved: identifiers starting with '{RESERVED_NAME_PREFIX}' are used by the compiler"
            ),
            span,
        ));
    }
    Ok(())
}

fn bind_generic_type(expected: &Type, actual: &Type, bindings: &mut HashMap<String, Type>) -> bool {
    match expected {
        Type::Generic(name) => match bindings.get(name) {
            Some(existing) => type_matches(existing, actual),
            None => {
                bindings.insert(name.clone(), actual.clone());
                true
            }
        },
        Type::Array(expected) => match actual {
            Type::Array(actual) => bind_generic_type(expected, actual, bindings),
            _ => false,
        },
        Type::Result(expected) => match actual {
            Type::Result(actual) => bind_generic_type(expected, actual, bindings),
            _ => false,
        },
        Type::Union(options) => options
            .iter()
            .any(|option| bind_generic_type(option, actual, bindings)),
        _ => true,
    }
}

fn substitute_generics(ty: &Type, bindings: &HashMap<String, Type>) -> Type {
    match ty {
        Type::Generic(name) => bindings.get(name).cloned().unwrap_or(Type::Unknown),
        Type::Array(inner) => Type::Array(Box::new(substitute_generics(inner, bindings))),
        Type::Result(inner) => Type::Result(Box::new(substitute_generics(inner, bindings))),
        Type::Union(types) => Type::Union(
            types
                .iter()
                .map(|ty| substitute_generics(ty, bindings))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn type_of_literal(literal: &Literal) -> Type {
    match literal {
        Literal::Int(_) => Type::Int,
        Literal::Float(_) => Type::Float,
        Literal::Bool(_) => Type::Bool,
        Literal::String(_) | Literal::TemplateString(_) => Type::String,
        Literal::Null => Type::Null,
    }
}

fn literal_key(literal: &Literal) -> String {
    match literal {
        Literal::Int(value) => format!("int:{value}"),
        Literal::Float(value) => format!("float:{value:?}"),
        Literal::Bool(value) => format!("bool:{value}"),
        Literal::String(value) | Literal::TemplateString(value) => format!("str:{value}"),
        Literal::Null => "null".to_string(),
    }
}

fn pattern_is_catch_all(pattern: &MatchPattern) -> bool {
    matches!(pattern, MatchPattern::Wildcard | MatchPattern::Binding(_))
}

fn enum_pattern_covers_all_payload(pattern: &MatchPattern) -> bool {
    let MatchPattern::EnumVariant { fields, .. } = pattern else {
        return false;
    };
    fields.iter().all(pattern_is_catch_all)
}

fn pattern_key(pattern: &MatchPattern) -> String {
    match pattern {
        MatchPattern::Wildcard => "_".to_string(),
        MatchPattern::Binding(_) => "$binding".to_string(),
        MatchPattern::Literal(literal) => literal_key(literal),
        MatchPattern::EnumVariant {
            enum_name,
            variant,
            fields,
        } => {
            let fields = fields.iter().map(pattern_key).collect::<Vec<_>>().join(",");
            format!("enum:{enum_name}.{variant}({fields})")
        }
    }
}

fn enum_variant_path(expr: &Expr) -> Option<(String, String)> {
    let ExprKind::Field { target, name } = &expr.kind else {
        return None;
    };
    let ExprKind::Variable(enum_name) = &target.kind else {
        return None;
    };
    Some((enum_name.clone(), name.clone()))
}

fn function_body_uses_name(body: &[Stmt], name: &str) -> bool {
    body.iter().any(|stmt| stmt_uses_name(stmt, name, false))
}

fn stmt_uses_name(stmt: &Stmt, name: &str, shadowed: bool) -> bool {
    match stmt {
        Stmt::VarDecl { value, .. } => expr_uses_name(value, name, shadowed),
        Stmt::Assign {
            name: assigned,
            value,
            ..
        } => (!shadowed && assigned == name) || expr_uses_name(value, name, shadowed),
        Stmt::AssignTarget { target, value, .. } | Stmt::CompoundAssign { target, value, .. } => {
            assign_target_uses_name(target, name, shadowed) || expr_uses_name(value, name, shadowed)
        }
        Stmt::DestructureAssign { values, .. } => values
            .iter()
            .any(|value| expr_uses_name(value, name, shadowed)),
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            expr_uses_name(value, name, shadowed)
                || bindings.iter().any(|binding| {
                    binding
                        .default
                        .as_ref()
                        .is_some_and(|default| expr_uses_name(default, name, shadowed))
                })
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_uses_name(condition, name, shadowed)
                || then_branch
                    .iter()
                    .any(|stmt| stmt_uses_name(stmt, name, shadowed))
                || else_branch
                    .iter()
                    .any(|stmt| stmt_uses_name(stmt, name, shadowed))
        }
        Stmt::While {
            condition, body, ..
        } => {
            expr_uses_name(condition, name, shadowed)
                || body.iter().any(|stmt| stmt_uses_name(stmt, name, shadowed))
        }
        Stmt::For {
            name: loop_name,
            iterable,
            body,
            ..
        } => {
            expr_uses_name(iterable, name, shadowed)
                || body
                    .iter()
                    .any(|stmt| stmt_uses_name(stmt, name, shadowed || loop_name == name))
        }
        Stmt::Function(function) => {
            let shadowed = shadowed
                || function.name == name
                || function.params.iter().any(|param| param.name == name);
            function
                .body
                .iter()
                .any(|stmt| stmt_uses_name(stmt, name, shadowed))
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            finally_body,
            ..
        } => {
            body.iter().any(|stmt| stmt_uses_name(stmt, name, shadowed))
                || catch_body.iter().any(|stmt| {
                    stmt_uses_name(stmt, name, shadowed || catch_name.as_deref() == Some(name))
                })
                || finally_body
                    .iter()
                    .any(|stmt| stmt_uses_name(stmt, name, shadowed))
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
            expr_uses_name(value, name, shadowed)
        }
        Stmt::Return { value, .. } => value
            .as_ref()
            .is_some_and(|value| expr_uses_name(value, name, shadowed)),
        Stmt::Expr { expr, .. } => expr_uses_name(expr, name, shadowed),
        Stmt::Break { .. } | Stmt::Continue { .. } => false,
    }
}

fn assign_target_uses_name(target: &AssignTarget, name: &str, shadowed: bool) -> bool {
    match target {
        AssignTarget::Variable(_) => false,
        AssignTarget::Index { target, index } => {
            expr_uses_name(target, name, shadowed) || expr_uses_name(index, name, shadowed)
        }
        AssignTarget::Field { target, .. } => expr_uses_name(target, name, shadowed),
    }
}

fn expr_uses_name(expr: &Expr, name: &str, shadowed: bool) -> bool {
    match &expr.kind {
        ExprKind::Variable(variable) => !shadowed && variable == name,
        ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
            expr_uses_name(expr, name, shadowed)
        }
        ExprKind::Binary { left, right, .. } => {
            expr_uses_name(left, name, shadowed) || expr_uses_name(right, name, shadowed)
        }
        ExprKind::Call { callee, args } => {
            expr_uses_name(callee, name, shadowed)
                || args.iter().any(|arg| expr_uses_name(arg, name, shadowed))
        }
        ExprKind::Array(values) => values
            .iter()
            .any(|value| expr_uses_name(value, name, shadowed)),
        ExprKind::Index { target, index } => {
            expr_uses_name(target, name, shadowed) || expr_uses_name(index, name, shadowed)
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            expr_uses_name(target, name, shadowed)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => fields
            .iter()
            .any(|(_, value)| expr_uses_name(value, name, shadowed)),
        ExprKind::Match { value, arms } => {
            expr_uses_name(value, name, shadowed)
                || arms.iter().any(|arm| {
                    let shadowed = shadowed || pattern_binds_name(&arm.pattern, name);
                    arm.guard
                        .as_ref()
                        .is_some_and(|guard| expr_uses_name(guard, name, shadowed))
                        || expr_uses_name(&arm.value, name, shadowed)
                })
        }
        ExprKind::Function { params, body, .. } => {
            let shadowed = shadowed || params.iter().any(|param| param.name == name);
            body.iter().any(|stmt| stmt_uses_name(stmt, name, shadowed))
        }
        ExprKind::Literal(_) => false,
    }
}

fn pattern_binds_name(pattern: &MatchPattern, name: &str) -> bool {
    match pattern {
        MatchPattern::Binding(binding) => binding == name,
        MatchPattern::EnumVariant { fields, .. } => {
            fields.iter().any(|field| pattern_binds_name(field, name))
        }
        MatchPattern::Wildcard | MatchPattern::Literal(_) => false,
    }
}

fn reject_http_side_effect_response_calls(body: &[Stmt], span: Span) -> KuResult<()> {
    for stmt in body {
        reject_http_side_effect_response_calls_in_stmt(stmt, span)?;
    }
    Ok(())
}

fn reject_http_side_effect_response_calls_in_stmt(stmt: &Stmt, span: Span) -> KuResult<()> {
    match stmt {
        Stmt::VarDecl { value, .. } | Stmt::Assign { value, .. } => {
            reject_http_side_effect_response_calls_in_expr(value, span)
        }
        Stmt::AssignTarget { target, value, .. } | Stmt::CompoundAssign { target, value, .. } => {
            reject_http_side_effect_response_calls_in_assign_target(target, span)?;
            reject_http_side_effect_response_calls_in_expr(value, span)
        }
        Stmt::DestructureAssign { values, .. } => {
            for value in values {
                reject_http_side_effect_response_calls_in_expr(value, span)?;
            }
            Ok(())
        }
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            reject_http_side_effect_response_calls_in_expr(value, span)?;
            for binding in bindings {
                if let Some(default) = &binding.default {
                    reject_http_side_effect_response_calls_in_expr(default, span)?;
                }
            }
            Ok(())
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            reject_http_side_effect_response_calls_in_expr(condition, span)?;
            reject_http_side_effect_response_calls_in_block(then_branch, span)?;
            reject_http_side_effect_response_calls_in_block(else_branch, span)
        }
        Stmt::While {
            condition, body, ..
        } => {
            reject_http_side_effect_response_calls_in_expr(condition, span)?;
            reject_http_side_effect_response_calls_in_block(body, span)
        }
        Stmt::For { iterable, body, .. } => {
            reject_http_side_effect_response_calls_in_expr(iterable, span)?;
            reject_http_side_effect_response_calls_in_block(body, span)
        }
        Stmt::Function(function) => {
            reject_http_side_effect_response_calls_in_block(&function.body, span)
        }
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            reject_http_side_effect_response_calls_in_block(body, span)?;
            reject_http_side_effect_response_calls_in_block(catch_body, span)?;
            reject_http_side_effect_response_calls_in_block(finally_body, span)
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
            reject_http_side_effect_response_calls_in_expr(value, span)
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                reject_http_side_effect_response_calls_in_expr(value, span)?;
            }
            Ok(())
        }
        Stmt::Expr { expr, .. } => reject_http_side_effect_response_calls_in_expr(expr, span),
        Stmt::Break { .. } | Stmt::Continue { .. } => Ok(()),
    }
}

fn reject_http_side_effect_response_calls_in_block(body: &[Stmt], span: Span) -> KuResult<()> {
    for stmt in body {
        reject_http_side_effect_response_calls_in_stmt(stmt, span)?;
    }
    Ok(())
}

fn reject_http_side_effect_response_calls_in_assign_target(
    target: &AssignTarget,
    span: Span,
) -> KuResult<()> {
    match target {
        AssignTarget::Variable(_) => Ok(()),
        AssignTarget::Index { target, index } => {
            reject_http_side_effect_response_calls_in_expr(target, span)?;
            reject_http_side_effect_response_calls_in_expr(index, span)
        }
        AssignTarget::Field { target, .. } => {
            reject_http_side_effect_response_calls_in_expr(target, span)
        }
    }
}

fn reject_http_side_effect_response_calls_in_expr(expr: &Expr, handler_span: Span) -> KuResult<()> {
    match &expr.kind {
        ExprKind::Call { callee, args } => {
            if let Some(name) = http_side_effect_response_call_name(callee) {
                return Err(KuError::runtime(
                    format!(
                        "ordinary HTTP handlers must return an HttpResponse; side-effect response API '{name}' is not allowed"
                    ),
                    expr.span,
                ));
            }
            reject_http_side_effect_response_calls_in_expr(callee, handler_span)?;
            for arg in args {
                reject_http_side_effect_response_calls_in_expr(arg, handler_span)?;
            }
            Ok(())
        }
        ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
            reject_http_side_effect_response_calls_in_expr(expr, handler_span)
        }
        ExprKind::Binary { left, right, .. } => {
            reject_http_side_effect_response_calls_in_expr(left, handler_span)?;
            reject_http_side_effect_response_calls_in_expr(right, handler_span)
        }
        ExprKind::Array(values) => {
            for value in values {
                reject_http_side_effect_response_calls_in_expr(value, handler_span)?;
            }
            Ok(())
        }
        ExprKind::Index { target, index } => {
            reject_http_side_effect_response_calls_in_expr(target, handler_span)?;
            reject_http_side_effect_response_calls_in_expr(index, handler_span)
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            reject_http_side_effect_response_calls_in_expr(target, handler_span)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                reject_http_side_effect_response_calls_in_expr(value, handler_span)?;
            }
            Ok(())
        }
        ExprKind::Match { value, arms } => {
            reject_http_side_effect_response_calls_in_expr(value, handler_span)?;
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    reject_http_side_effect_response_calls_in_expr(guard, handler_span)?;
                }
                reject_http_side_effect_response_calls_in_expr(&arm.value, handler_span)?;
            }
            Ok(())
        }
        ExprKind::Function { body, .. } => {
            reject_http_side_effect_response_calls_in_block(body, handler_span)
        }
        ExprKind::Literal(_) | ExprKind::Variable(_) => Ok(()),
    }
}

fn http_side_effect_response_call_name(callee: &Expr) -> Option<String> {
    let ExprKind::Field { target, name } = &callee.kind else {
        return None;
    };
    let root = expr_root_name(target)?;
    let blocked = matches!(
        (root, name.as_str()),
        ("res", "write" | "end" | "status" | "header")
            | ("reply", "send" | "write" | "end")
            | ("writer", "write" | "status" | "header" | "end")
    );
    blocked.then(|| format!("{root}.{name}"))
}

fn type_error(span: Span, expected: &Type, actual: &Type) -> KuError {
    KuError::runtime(
        format!(
            "type error: expected {} but got {}",
            type_name(expected),
            type_name(actual)
        ),
        span,
    )
}

fn http_handler_return_error(span: Span, actual: &Type) -> KuError {
    KuError::runtime(
        format!(
            "HTTP handler must return HttpResponse or HttpResponse!, but got {}",
            type_name(actual)
        ),
        span,
    )
}

fn expect_type_arg(actual: &Type, expected: &Type, span: Span) -> KuResult<()> {
    if type_matches(expected, actual) {
        Ok(())
    } else {
        Err(type_error(span, expected, actual))
    }
}

fn expect_dynamic_object_arg(label: &str, actual: &Type, span: Span) -> KuResult<()> {
    if matches!(
        actual,
        Type::DynamicObject | Type::Object(_) | Type::Unknown
    ) {
        Ok(())
    } else {
        Err(KuError::runtime(
            format!(
                "{label} expects a time object but got {}",
                type_name(actual)
            ),
            span,
        ))
    }
}

fn expect_arg_count_range(
    name: &str,
    actual: usize,
    min: usize,
    max: usize,
    span: Span,
) -> KuResult<()> {
    if (min..=max).contains(&actual) {
        Ok(())
    } else if min == max {
        Err(KuError::runtime(
            format!("function '{name}' expects {min} arguments but got {actual}"),
            span,
        ))
    } else {
        Err(KuError::runtime(
            format!("function '{name}' expects {min} to {max} arguments but got {actual}"),
            span,
        ))
    }
}

fn error_type() -> Type {
    Type::Object(HashMap::from([
        ("domain".to_string(), Type::String),
        ("code".to_string(), Type::String),
        ("message".to_string(), Type::String),
    ]))
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::Int => "int".to_string(),
        Type::Float => "float".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "str".to_string(),
        Type::Null => "null".to_string(),
        Type::Array(inner) => format!("[{}]", type_name(inner)),
        Type::Result(inner) => format!("{}!", type_name(inner)),
        Type::Task(inner) => format!("Task<{}>", type_name(inner)),
        Type::Union(types) => types.iter().map(type_name).collect::<Vec<_>>().join(" | "),
        Type::Object(_) => "object".to_string(),
        Type::StringMap => "object".to_string(),
        Type::DynamicObject => "object".to_string(),
        Type::KuValue => "KuValue".to_string(),
        Type::Struct(name) => name.clone(),
        Type::Enum(name) => name.clone(),
        Type::Native(name) => name.trim_start_matches("__ku_").to_string(),
        Type::Generic(name) => name.clone(),
        Type::Void => "void".to_string(),
        Type::FunctionValue {
            params,
            return_type,
            is_async,
            ..
        } => {
            let params = params
                .iter()
                .map(|param| {
                    param
                        .ty
                        .as_ref()
                        .map(type_name)
                        .unwrap_or_else(|| "unknown".to_string())
                })
                .collect::<Vec<_>>()
                .join(", ");
            let returns = return_type
                .as_deref()
                .map(type_name)
                .unwrap_or_else(|| "unknown".to_string());
            if *is_async {
                format!("async fn({params}): {returns}")
            } else {
                format!("fn({params}): {returns}")
            }
        }
        Type::Unknown => "unknown".to_string(),
    }
}

fn contains_task_type(ty: &Type) -> bool {
    match ty {
        Type::Task(_) => true,
        Type::Array(inner) | Type::Result(inner) => contains_task_type(inner),
        Type::Object(fields) => fields.values().any(contains_task_type),
        Type::FunctionValue {
            params,
            return_type,
            ..
        } => {
            params
                .iter()
                .any(|param| param.ty.as_ref().is_some_and(contains_task_type))
                || return_type.as_deref().is_some_and(contains_task_type)
        }
        Type::Union(types) => types.iter().any(contains_task_type),
        _ => false,
    }
}

fn function_value_type(name: &str, function: &FunctionType, span: Span) -> KuResult<Type> {
    if !function.type_params.is_empty() {
        return Err(KuError::runtime(
            format!("generic function '{name}' cannot be used as a function value yet"),
            span,
        ));
    }
    Ok(Type::FunctionValue {
        params: function.value_params.clone(),
        return_type: function.return_type.clone().map(Box::new),
        body: function.body.clone(),
        body_id: Some(function.body_id),
        is_async: function.is_async,
    })
}

fn can_template_concat(left: &Type, right: &Type) -> bool {
    if let Type::Union(types) = left {
        return types.iter().all(|ty| can_template_concat(ty, right));
    }
    if let Type::Union(types) = right {
        return types.iter().all(|ty| can_template_concat(left, ty));
    }
    matches!(
        (left, right),
        (Type::String, Type::Int | Type::Float)
            | (Type::Int | Type::Float, Type::String)
            | (Type::String, Type::String)
    )
}

/// Dataflow join of a projection path's move mark across control-flow branches.
/// `branch_marks[i]` is the path's mark in branch `i` (`None` = live there).
/// The path is `Moved` only when it is definitely `Moved` in *every* branch;
/// moved on some paths (or `MaybeMoved` in any) yields `MaybeMoved`; live in all
/// yields `None`.
fn join_move_marks(branch_marks: &[Option<MoveMark>]) -> Option<MoveMark> {
    let branch_count = branch_marks.len();
    let present = branch_marks.iter().filter(|m| m.is_some()).count();
    if present == 0 {
        return None;
    }
    if present == branch_count && branch_marks.iter().all(|m| *m == Some(MoveMark::Moved)) {
        Some(MoveMark::Moved)
    } else {
        Some(MoveMark::MaybeMoved)
    }
}

fn merge_optional_closure_provenance(
    target: &mut Option<ClosureProvenance>,
    incoming: Option<ClosureProvenance>,
) {
    let Some(incoming) = incoming else {
        return;
    };
    if let Some(existing) = target {
        existing.merge(&incoming);
    } else {
        *target = Some(incoming);
    }
}

fn checker_closure_capture_names(
    params: &[FunctionParam],
    body: &[Stmt],
    visible_names: &HashSet<String>,
) -> HashSet<String> {
    let mut bound = params
        .iter()
        .map(|param| param.name.clone())
        .collect::<HashSet<_>>();
    let mut captures = HashSet::new();
    collect_checker_capture_block(body, &mut bound, visible_names, &mut captures);
    captures
}

fn checker_local_function_capture_names(
    function: &FnDecl,
    visible_names: &HashSet<String>,
) -> HashSet<String> {
    let mut bound = function
        .params
        .iter()
        .map(|param| param.name.clone())
        .collect::<HashSet<_>>();
    bound.insert(function.name.clone());
    let mut captures = HashSet::new();
    collect_checker_capture_block(&function.body, &mut bound, visible_names, &mut captures);
    captures
}

fn collect_checker_capture_block(
    body: &[Stmt],
    bound: &mut HashSet<String>,
    visible_names: &HashSet<String>,
    captures: &mut HashSet<String>,
) {
    for stmt in body {
        collect_checker_capture_stmt(stmt, bound, visible_names, captures);
    }
}

fn capture_checker_name(
    name: &str,
    bound: &HashSet<String>,
    visible_names: &HashSet<String>,
    captures: &mut HashSet<String>,
) {
    if !bound.contains(name) && visible_names.contains(name) {
        captures.insert(name.to_string());
    }
}

fn bind_or_capture_checker_name(
    name: &str,
    bound: &mut HashSet<String>,
    visible_names: &HashSet<String>,
    captures: &mut HashSet<String>,
) {
    if bound.contains(name) {
        return;
    }
    if visible_names.contains(name) {
        captures.insert(name.to_string());
    } else {
        bound.insert(name.to_string());
    }
}

fn collect_checker_capture_stmt(
    stmt: &Stmt,
    bound: &mut HashSet<String>,
    visible_names: &HashSet<String>,
    captures: &mut HashSet<String>,
) {
    match stmt {
        Stmt::VarDecl { name, value, .. } => {
            collect_checker_capture_expr(value, bound, visible_names, captures);
            bound.insert(name.clone());
        }
        Stmt::Assign { name, value, .. } => {
            collect_checker_capture_expr(value, bound, visible_names, captures);
            bind_or_capture_checker_name(name, bound, visible_names, captures);
        }
        Stmt::AssignTarget { target, value, .. } | Stmt::CompoundAssign { target, value, .. } => {
            collect_checker_capture_assign_target(target, bound, visible_names, captures);
            collect_checker_capture_expr(value, bound, visible_names, captures);
        }
        Stmt::DestructureAssign { names, values, .. } => {
            for value in values {
                collect_checker_capture_expr(value, bound, visible_names, captures);
            }
            for name in names.iter().flatten() {
                bind_or_capture_checker_name(name, bound, visible_names, captures);
            }
        }
        Stmt::ObjectDestructureAssign {
            bindings,
            rest,
            value,
            ..
        } => {
            collect_checker_capture_expr(value, bound, visible_names, captures);
            for binding in bindings {
                if let Some(default) = &binding.default {
                    collect_checker_capture_expr(default, bound, visible_names, captures);
                }
                if let Some(local) = &binding.local {
                    bind_or_capture_checker_name(local, bound, visible_names, captures);
                }
            }
            if let Some(local) = rest.as_ref().and_then(|rest| rest.local.as_ref()) {
                bind_or_capture_checker_name(local, bound, visible_names, captures);
            }
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_checker_capture_expr(condition, bound, visible_names, captures);
            collect_checker_capture_block(then_branch, &mut bound.clone(), visible_names, captures);
            collect_checker_capture_block(else_branch, &mut bound.clone(), visible_names, captures);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_checker_capture_expr(condition, bound, visible_names, captures);
            collect_checker_capture_block(body, &mut bound.clone(), visible_names, captures);
        }
        Stmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            collect_checker_capture_expr(iterable, bound, visible_names, captures);
            let mut nested = bound.clone();
            nested.insert(name.clone());
            collect_checker_capture_block(body, &mut nested, visible_names, captures);
        }
        Stmt::Function(function) => {
            let mut nested = bound.clone();
            nested.insert(function.name.clone());
            nested.extend(function.params.iter().map(|param| param.name.clone()));
            collect_checker_capture_block(&function.body, &mut nested, visible_names, captures);
            bound.insert(function.name.clone());
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            finally_body,
            ..
        } => {
            collect_checker_capture_block(body, &mut bound.clone(), visible_names, captures);
            let mut catch_bound = bound.clone();
            if let Some(name) = catch_name {
                catch_bound.insert(name.clone());
            }
            collect_checker_capture_block(catch_body, &mut catch_bound, visible_names, captures);
            collect_checker_capture_block(
                finally_body,
                &mut bound.clone(),
                visible_names,
                captures,
            );
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
            collect_checker_capture_expr(value, bound, visible_names, captures);
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_checker_capture_expr(value, bound, visible_names, captures);
            }
        }
        Stmt::Expr { expr, .. } => {
            collect_checker_capture_expr(expr, bound, visible_names, captures);
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
    }
}

fn collect_checker_capture_assign_target(
    target: &AssignTarget,
    bound: &HashSet<String>,
    visible_names: &HashSet<String>,
    captures: &mut HashSet<String>,
) {
    match target {
        AssignTarget::Variable(name) => {
            capture_checker_name(name, bound, visible_names, captures);
        }
        AssignTarget::Index { target, index } => {
            collect_checker_capture_expr(target, bound, visible_names, captures);
            collect_checker_capture_expr(index, bound, visible_names, captures);
        }
        AssignTarget::Field { target, .. } => {
            collect_checker_capture_expr(target, bound, visible_names, captures);
        }
    }
}

fn collect_checker_capture_expr(
    expr: &Expr,
    bound: &HashSet<String>,
    visible_names: &HashSet<String>,
    captures: &mut HashSet<String>,
) {
    match &expr.kind {
        ExprKind::Variable(name) => {
            capture_checker_name(name, bound, visible_names, captures);
        }
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => {
            collect_checker_capture_expr(expr, bound, visible_names, captures);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_checker_capture_expr(left, bound, visible_names, captures);
            collect_checker_capture_expr(right, bound, visible_names, captures);
        }
        ExprKind::Call { callee, args } => {
            collect_checker_capture_expr(callee, bound, visible_names, captures);
            for arg in args {
                collect_checker_capture_expr(arg, bound, visible_names, captures);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_checker_capture_expr(value, bound, visible_names, captures);
            }
        }
        ExprKind::Index { target, index } => {
            collect_checker_capture_expr(target, bound, visible_names, captures);
            collect_checker_capture_expr(index, bound, visible_names, captures);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_checker_capture_expr(target, bound, visible_names, captures);
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_checker_capture_expr(value, bound, visible_names, captures);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_checker_capture_expr(value, bound, visible_names, captures);
            for arm in arms {
                let mut arm_bound = bound.clone();
                bind_match_pattern_names(&arm.pattern, &mut arm_bound);
                if let Some(guard) = &arm.guard {
                    collect_checker_capture_expr(guard, &arm_bound, visible_names, captures);
                }
                collect_checker_capture_expr(&arm.value, &arm_bound, visible_names, captures);
            }
        }
        ExprKind::Function { params, body, .. } => {
            let mut nested = bound.clone();
            nested.extend(params.iter().map(|param| param.name.clone()));
            collect_checker_capture_block(body, &mut nested, visible_names, captures);
        }
        ExprKind::Literal(_) => {}
    }
}

fn bind_match_pattern_names(pattern: &MatchPattern, bound: &mut HashSet<String>) {
    match pattern {
        MatchPattern::Binding(name) => {
            bound.insert(name.clone());
        }
        MatchPattern::EnumVariant { fields, .. } => {
            for field in fields {
                bind_match_pattern_names(field, bound);
            }
        }
        MatchPattern::Wildcard | MatchPattern::Literal(_) => {}
    }
}

fn collect_effect_expression_outer_capture_ids(
    expr: &Expr,
    outer_bindings: &HashMap<String, BindingId>,
    provenance: &mut ClosureProvenance,
) {
    match &expr.kind {
        ExprKind::Function { params, body, .. } => {
            let visible_names = outer_bindings.keys().cloned().collect::<HashSet<_>>();
            for name in checker_closure_capture_names(params, body, &visible_names) {
                if let Some(binding_id) = outer_bindings.get(&name) {
                    provenance.dependencies.insert(*binding_id);
                }
            }
        }
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => {
            collect_effect_expression_outer_capture_ids(expr, outer_bindings, provenance);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_effect_expression_outer_capture_ids(left, outer_bindings, provenance);
            collect_effect_expression_outer_capture_ids(right, outer_bindings, provenance);
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_effect_expression_outer_capture_ids(value, outer_bindings, provenance);
            }
        }
        ExprKind::Index { target, index } => {
            collect_effect_expression_outer_capture_ids(target, outer_bindings, provenance);
            collect_effect_expression_outer_capture_ids(index, outer_bindings, provenance);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_effect_expression_outer_capture_ids(target, outer_bindings, provenance);
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_effect_expression_outer_capture_ids(value, outer_bindings, provenance);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_effect_expression_outer_capture_ids(value, outer_bindings, provenance);
            for arm in arms {
                collect_effect_expression_outer_capture_ids(&arm.value, outer_bindings, provenance);
            }
        }
        // Calls are handled by `effect_expression_closure_provenance`, which can
        // distinguish Identity from Discard instead of retaining every argument.
        ExprKind::Call { .. } | ExprKind::Literal(_) | ExprKind::Variable(_) => {}
    }
}

fn bind_match_pattern_closure_provenance(
    pattern: &MatchPattern,
    selected: &ClosureProvenance,
    symbolic: &mut HashMap<String, ClosureProvenance>,
) {
    match pattern {
        MatchPattern::Binding(name) => {
            symbolic.insert(name.clone(), selected.clone());
        }
        MatchPattern::EnumVariant { fields, .. } => {
            for field in fields {
                bind_match_pattern_closure_provenance(field, selected, symbolic);
            }
        }
        MatchPattern::Wildcard | MatchPattern::Literal(_) => {}
    }
}

fn expr_may_call_function(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Call { .. } | ExprKind::Await(_) => true,
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } => expr_may_call_function(expr),
        ExprKind::Binary { left, right, .. } => {
            expr_may_call_function(left) || expr_may_call_function(right)
        }
        ExprKind::Array(values) => values.iter().any(expr_may_call_function),
        ExprKind::Index { target, index } => {
            expr_may_call_function(target) || expr_may_call_function(index)
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            expr_may_call_function(target)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => fields
            .iter()
            .any(|(_, value)| expr_may_call_function(value)),
        ExprKind::Match { value, arms } => {
            expr_may_call_function(value)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(expr_may_call_function)
                        || expr_may_call_function(&arm.value)
                })
        }
        // Creating a closure does not execute its body.
        ExprKind::Function { .. } | ExprKind::Literal(_) | ExprKind::Variable(_) => false,
    }
}

fn merge_symbolic_fallthrough(
    left: Option<HashMap<String, ClosureProvenance>>,
    right: Option<HashMap<String, ClosureProvenance>>,
) -> Option<HashMap<String, ClosureProvenance>> {
    match (left, right) {
        (None, None) => None,
        (Some(environment), None) | (None, Some(environment)) => Some(environment),
        (Some(left), Some(right)) => {
            let mut merged = HashMap::new();
            for (name, mut provenance) in left {
                if let Some(right_provenance) = right.get(&name) {
                    provenance.merge(right_provenance);
                    merged.insert(name, provenance);
                }
            }
            Some(merged)
        }
    }
}

fn restore_symbolic_block_scope(
    mut flow: ClosureReturnFlow,
    entry: &HashMap<String, ClosureProvenance>,
    body: &[Stmt],
) -> ClosureReturnFlow {
    let Some(fallthrough) = flow.fallthrough.as_mut() else {
        return flow;
    };
    for stmt in body {
        let Stmt::VarDecl { name, .. } = stmt else {
            continue;
        };
        if let Some(previous) = entry.get(name) {
            fallthrough.insert(name.clone(), previous.clone());
        } else {
            fallthrough.remove(name);
        }
    }
    flow
}

fn merge_function_value_branch_types(base: &Type, branches: &[Option<&Type>]) -> Type {
    let Type::FunctionValue {
        params,
        return_type,
        body_id: base_body_id,
        is_async,
        ..
    } = base
    else {
        return base.clone();
    };
    let branch_types = branches
        .iter()
        .map(|branch| (*branch).unwrap_or(base))
        .collect::<Vec<_>>();
    let first_body_id = branch_types.first().and_then(|ty| match ty {
        Type::FunctionValue { body_id, .. } => Some(*body_id),
        _ => None,
    });
    let same_body = first_body_id.is_some()
        && branch_types.iter().all(|ty| {
            matches!(ty, Type::FunctionValue { body_id, .. } if Some(*body_id) == first_body_id)
        });
    if same_body {
        return branch_types[0].clone();
    }
    // Different runtime call targets reach the join. Keep the checked signature,
    // but erase the concrete body so an indirect-call provenance query takes its
    // conservative unknown+arguments path instead of auditing the wrong branch.
    Type::FunctionValue {
        params: params.clone(),
        return_type: return_type.clone(),
        body: Vec::new(),
        body_id: if branches.is_empty() {
            *base_body_id
        } else {
            None
        },
        is_async: *is_async,
    }
}

/// Merge the per-path move state of one variable across the branch scopes.
fn merge_var_moves(var: &mut VarType, branch_moves: &[Option<&BTreeMap<Vec<String>, MoveMark>>]) {
    let mut paths: std::collections::BTreeSet<Vec<String>> = std::collections::BTreeSet::new();
    for moves in branch_moves.iter().flatten() {
        paths.extend(moves.keys().cloned());
    }
    let mut merged = BTreeMap::new();
    for path in paths {
        let marks: Vec<Option<MoveMark>> = branch_moves
            .iter()
            .map(|moves| moves.and_then(|m| m.get(&path)).copied())
            .collect();
        if let Some(mark) = join_move_marks(&marks) {
            merged.insert(path, mark);
        }
    }
    var.moves = merged;
}

/// Merge the two branch states of an `if` into the state that reaches the code
/// after it. A branch that DIVERGES (ends in `return`/`break`/`continue`/`fail`/
/// `panic`) never falls through, so its moves must not reach the fall-through
/// path — only the state(s) that can actually reach the join contribute. Without
/// this a guard clause like `if (bad) { take(x); return }` would poison `x` for
/// the code after the `if`, where `x` is in fact still live.
fn merge_if_scopes(
    before: Vec<HashMap<String, VarType>>,
    then_scopes: Vec<HashMap<String, VarType>>,
    else_scopes: Vec<HashMap<String, VarType>>,
    then_falls: bool,
    else_falls: bool,
) -> Vec<HashMap<String, VarType>> {
    match (then_falls, else_falls) {
        (true, true) => merge_moved_scopes(before, then_scopes, else_scopes),
        (true, false) => then_scopes,
        (false, true) => else_scopes,
        // Both branches diverge: the code after the `if` is unreachable. Keep the
        // pre-`if` state so it is checked as if the branches never ran.
        (false, false) => before,
    }
}

fn merge_moved_scopes(
    mut base: Vec<HashMap<String, VarType>>,
    then_scopes: Vec<HashMap<String, VarType>>,
    else_scopes: Vec<HashMap<String, VarType>>,
) -> Vec<HashMap<String, VarType>> {
    for (index, scope) in base.iter_mut().enumerate() {
        for (name, var) in scope.iter_mut() {
            let then_var = then_scopes.get(index).and_then(|scope| scope.get(name));
            let else_var = else_scopes.get(index).and_then(|scope| scope.get(name));
            let then_moves = then_var.map(|value| &value.moves);
            let else_moves = else_var.map(|value| &value.moves);
            merge_var_moves(var, &[then_moves, else_moves]);
            var.ty = merge_function_value_branch_types(
                &var.ty,
                &[
                    then_var.map(|value| &value.ty),
                    else_var.map(|value| &value.ty),
                ],
            );
            let mut provenance = ClosureProvenance::empty();
            if let Some(then_var) = then_var {
                debug_assert_eq!(then_var.binding_id, var.binding_id);
                provenance.merge(&then_var.closure_provenance);
            } else {
                provenance.merge(&var.closure_provenance);
            }
            if let Some(else_var) = else_var {
                debug_assert_eq!(else_var.binding_id, var.binding_id);
                provenance.merge(&else_var.closure_provenance);
            } else {
                provenance.merge(&var.closure_provenance);
            }
            var.closure_provenance = provenance;
            // A closure created on either reachable path may escape that path and
            // continue reading the shared cell. Unlike a move mark, capture is
            // monotonic: assignment cannot make the captured binding unboxed.
            var.captured |= then_var.is_some_and(|value| value.captured)
                || else_var.is_some_and(|value| value.captured);
        }
    }
    base
}

fn merge_moved_scope_paths(
    mut base: Vec<HashMap<String, VarType>>,
    paths: &[Vec<HashMap<String, VarType>>],
) -> Vec<HashMap<String, VarType>> {
    for (index, scope) in base.iter_mut().enumerate() {
        for (name, var) in scope.iter_mut() {
            let captured = paths.iter().any(|path| {
                path.get(index)
                    .and_then(|scope| scope.get(name))
                    .is_some_and(|value| value.captured)
            });
            let branch_moves: Vec<Option<&BTreeMap<Vec<String>, MoveMark>>> = paths
                .iter()
                .map(|path| {
                    path.get(index)
                        .and_then(|scope| scope.get(name))
                        .map(|value| &value.moves)
                })
                .collect();
            merge_var_moves(var, &branch_moves);
            let branch_types = paths
                .iter()
                .map(|path| {
                    path.get(index)
                        .and_then(|scope| scope.get(name))
                        .map(|value| &value.ty)
                })
                .collect::<Vec<_>>();
            var.ty = merge_function_value_branch_types(&var.ty, &branch_types);
            let mut provenance = ClosureProvenance::empty();
            for path in paths {
                if let Some(path_var) = path.get(index).and_then(|scope| scope.get(name)) {
                    debug_assert_eq!(path_var.binding_id, var.binding_id);
                    provenance.merge(&path_var.closure_provenance);
                } else {
                    provenance.merge(&var.closure_provenance);
                }
            }
            var.closure_provenance = provenance;
            var.captured |= captured;
        }
    }
    base
}

fn merge_return_types(left: &Type, right: &Type, span: Span) -> KuResult<Type> {
    if left == &Type::Null {
        return Ok(right.clone());
    }
    if right == &Type::Null {
        return Ok(left.clone());
    }
    if type_matches(left, right) {
        Ok(left.clone())
    } else {
        Err(type_error(span, left, right))
    }
}

fn is_constant_name(name: &str) -> bool {
    let mut has_alpha = false;
    for ch in name.chars() {
        if ch.is_ascii_alphabetic() {
            has_alpha = true;
            if ch.is_ascii_lowercase() {
                return false;
            }
        } else if ch != '_' && !ch.is_ascii_digit() {
            return false;
        }
    }
    has_alpha
}

fn block_may_return(body: &[Stmt]) -> bool {
    body.iter().any(stmt_may_return)
}

fn stmt_may_return(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return { .. } | Stmt::Fail { .. } => true,
        Stmt::Break { .. } | Stmt::Continue { .. } => false,
        Stmt::If {
            then_branch,
            else_branch,
            ..
        } => {
            !else_branch.is_empty()
                && block_may_return(then_branch)
                && block_may_return(else_branch)
        }
        _ => false,
    }
}

fn stmt_stops_fallthrough(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Return { .. }
        | Stmt::Fail { .. }
        | Stmt::Panic { .. } => true,
        Stmt::If {
            then_branch,
            else_branch,
            ..
        } => {
            !else_branch.is_empty()
                && block_stops_fallthrough(then_branch)
                && block_stops_fallthrough(else_branch)
        }
        // `while (true) { ... }` with no `break` that exits it never falls through.
        Stmt::While {
            condition, body, ..
        } => {
            matches!(&condition.kind, ExprKind::Literal(Literal::Bool(true)))
                && !block_has_own_break(body)
        }
        _ => false,
    }
}

/// True when `body` contains a `break` that would exit the enclosing loop (a
/// `break` inside a NESTED loop targets that loop, not this one, so nested loops
/// are not descended into).
fn block_has_own_break(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_own_break)
}

fn stmt_has_own_break(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Break { .. } => true,
        Stmt::If {
            then_branch,
            else_branch,
            ..
        } => block_has_own_break(then_branch) || block_has_own_break(else_branch),
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            block_has_own_break(body)
                || block_has_own_break(catch_body)
                || block_has_own_break(finally_body)
        }
        // Nested loops capture their own `break`.
        _ => false,
    }
}

fn block_stops_fallthrough(body: &[Stmt]) -> bool {
    body.iter().any(stmt_stops_fallthrough)
}

#[derive(Clone, Copy)]
struct LoopBodyFlow {
    fallthrough: bool,
    continues: bool,
}

fn loop_body_has_backedge(body: &[Stmt]) -> bool {
    let mut flow = LoopBodyFlow {
        fallthrough: true,
        continues: false,
    };
    for stmt in body {
        if !flow.fallthrough {
            break;
        }
        let next = loop_stmt_flow(stmt);
        flow.continues |= next.continues;
        flow.fallthrough = next.fallthrough;
    }
    flow.fallthrough || flow.continues
}

fn loop_stmt_flow(stmt: &Stmt) -> LoopBodyFlow {
    match stmt {
        Stmt::Continue { .. } => LoopBodyFlow {
            fallthrough: false,
            continues: true,
        },
        Stmt::Break { .. } | Stmt::Return { .. } | Stmt::Fail { .. } | Stmt::Panic { .. } => {
            LoopBodyFlow {
                fallthrough: false,
                continues: false,
            }
        }
        Stmt::If {
            then_branch,
            else_branch,
            ..
        } => {
            let then_flow = loop_block_flow(then_branch);
            let else_flow = if else_branch.is_empty() {
                LoopBodyFlow {
                    fallthrough: true,
                    continues: false,
                }
            } else {
                loop_block_flow(else_branch)
            };
            LoopBodyFlow {
                fallthrough: then_flow.fallthrough || else_flow.fallthrough,
                continues: then_flow.continues || else_flow.continues,
            }
        }
        Stmt::While { .. } | Stmt::For { .. } => LoopBodyFlow {
            fallthrough: true,
            continues: false,
        },
        _ => LoopBodyFlow {
            fallthrough: true,
            continues: false,
        },
    }
}

fn loop_block_flow(body: &[Stmt]) -> LoopBodyFlow {
    let mut flow = LoopBodyFlow {
        fallthrough: true,
        continues: false,
    };
    for stmt in body {
        if !flow.fallthrough {
            break;
        }
        let next = loop_stmt_flow(stmt);
        flow.continues |= next.continues;
        flow.fallthrough = next.fallthrough;
    }
    flow
}

fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::VarDecl { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::AssignTarget { span, .. }
        | Stmt::CompoundAssign { span, .. }
        | Stmt::DestructureAssign { span, .. }
        | Stmt::ObjectDestructureAssign { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Break { span }
        | Stmt::Continue { span }
        | Stmt::Function(FnDecl { span, .. })
        | Stmt::Try { span, .. }
        | Stmt::Fail { span, .. }
        | Stmt::Panic { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Print { span, .. }
        | Stmt::Expr { span, .. } => *span,
    }
}

fn dotted_name(expr: &Expr) -> Option<(String, String)> {
    let ExprKind::Field { target, name } = &expr.kind else {
        return None;
    };
    let ExprKind::Variable(module) = &target.kind else {
        return None;
    };
    Some((module.clone(), name.clone()))
}

fn http_client_type() -> Type {
    Type::Object(HashMap::from([
        ("kind".to_string(), Type::String),
        ("timeout_ms".to_string(), Type::Int),
        ("max_body_bytes".to_string(), Type::Int),
    ]))
}

const REDIS_CLIENT_CONFIG_FIELDS: [&str; 9] = [
    "host",
    "port",
    "username",
    "password",
    "max_connections",
    "max_waiters",
    "connect_timeout_ms",
    "acquire_timeout_ms",
    "command_timeout_ms",
];

fn validate_redis_client_config(config_type: &Type, span: Span) -> KuResult<()> {
    let Type::Object(fields) = config_type else {
        // Dynamic configuration is validated again by the native constructor.
        return Ok(());
    };
    let mut unknown = fields
        .keys()
        .filter(|key| !REDIS_CLIENT_CONFIG_FIELDS.contains(&key.as_str()))
        .collect::<Vec<_>>();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Err(KuError::runtime(
            format!("unknown redis client config field '{key}'"),
            span,
        ));
    }
    let Some(host) = fields.get("host") else {
        return Err(KuError::runtime(
            "redis.client config requires string field 'host'",
            span,
        ));
    };
    if *host != Type::String {
        return Err(type_error(span, &Type::String, host));
    }
    for name in [
        "port",
        "max_connections",
        "max_waiters",
        "connect_timeout_ms",
        "acquire_timeout_ms",
        "command_timeout_ms",
    ] {
        if let Some(actual) = fields.get(name) {
            if *actual != Type::Int {
                return Err(KuError::runtime(
                    format!("redis.client config field '{name}' must be int"),
                    span,
                ));
            }
        }
    }
    for name in ["username", "password"] {
        if let Some(actual) = fields.get(name) {
            if *actual != Type::String {
                return Err(KuError::runtime(
                    format!("redis.client config field '{name}' must be str"),
                    span,
                ));
            }
        }
    }
    if fields.contains_key("username") && !fields.contains_key("password") {
        return Err(KuError::runtime(
            "redis.client config field 'username' requires 'password'",
            span,
        ));
    }
    Ok(())
}

const NET_CLIENT_CONFIG_FIELDS: [&str; 9] = [
    "host",
    "port",
    "connect_timeout_ms",
    "read_timeout_ms",
    "write_timeout_ms",
    "max_read_bytes",
    "tls",
    "tls_server_name",
    "tls_ca_pem",
];

fn validate_net_client_config(config_type: &Type, span: Span) -> KuResult<()> {
    let Type::Object(fields) = config_type else {
        // Dynamic configuration is validated again by the native constructor.
        return Ok(());
    };
    let mut unknown = fields
        .keys()
        .filter(|key| !NET_CLIENT_CONFIG_FIELDS.contains(&key.as_str()))
        .collect::<Vec<_>>();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Err(KuError::runtime(
            format!("unknown net client config field '{key}'"),
            span,
        ));
    }
    let Some(host) = fields.get("host") else {
        return Err(KuError::runtime(
            "net.client config requires string field 'host'",
            span,
        ));
    };
    if *host != Type::String {
        return Err(type_error(span, &Type::String, host));
    }
    let Some(port) = fields.get("port") else {
        return Err(KuError::runtime(
            "net.client config requires int field 'port'",
            span,
        ));
    };
    if *port != Type::Int {
        return Err(KuError::runtime(
            "net.client config field 'port' must be int",
            span,
        ));
    }
    for name in [
        "connect_timeout_ms",
        "read_timeout_ms",
        "write_timeout_ms",
        "max_read_bytes",
    ] {
        if let Some(actual) = fields.get(name) {
            if *actual != Type::Int {
                return Err(KuError::runtime(
                    format!("net.client config field '{name}' must be int"),
                    span,
                ));
            }
        }
    }
    if let Some(actual) = fields.get("tls") {
        if *actual != Type::Bool {
            return Err(KuError::runtime(
                "net.client config field 'tls' must be bool",
                span,
            ));
        }
    }
    for name in ["tls_server_name", "tls_ca_pem"] {
        if let Some(actual) = fields.get(name) {
            if *actual != Type::String {
                return Err(KuError::runtime(
                    format!("net.client config field '{name}' must be str"),
                    span,
                ));
            }
        }
    }
    Ok(())
}

fn validate_net_tls_literal(config: &Expr) -> KuResult<()> {
    let ExprKind::ObjectLiteral { fields } = &config.kind else {
        return Ok(());
    };
    let tls = fields
        .iter()
        .find(|(name, _)| name == "tls")
        .map(|(_, value)| value);
    let tls_is_disabled = match tls {
        None => true,
        Some(value) => matches!(value.kind, ExprKind::Literal(Literal::Bool(false))),
    };
    if !tls_is_disabled {
        return Ok(());
    }
    for name in ["tls_server_name", "tls_ca_pem"] {
        if fields.iter().any(|(field_name, _)| field_name == name) {
            return Err(KuError::runtime(
                format!("net.client config field '{name}' requires 'tls' to be true"),
                config.span,
            ));
        }
    }
    Ok(())
}

const MYSQL_CLIENT_CONFIG_FIELDS: [&str; 10] = [
    "host",
    "port",
    "user",
    "password",
    "database",
    "max_connections",
    "max_waiters",
    "connect_timeout_ms",
    "acquire_timeout_ms",
    "query_timeout_ms",
];

fn validate_mysql_client_config(config_type: &Type, span: Span) -> KuResult<()> {
    let Type::Object(fields) = config_type else {
        // Values flowing from a dynamic object are checked by the native
        // constructor with the same field names and bounds.
        return Ok(());
    };
    let mut unknown = fields
        .keys()
        .filter(|key| !MYSQL_CLIENT_CONFIG_FIELDS.contains(&key.as_str()))
        .collect::<Vec<_>>();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Err(KuError::runtime(
            format!("unknown mysql client config field '{key}'"),
            span,
        ));
    }
    for name in ["host", "user", "password", "database"] {
        let Some(actual) = fields.get(name) else {
            return Err(KuError::runtime(
                format!("mysql.client config requires string field '{name}'"),
                span,
            ));
        };
        if *actual != Type::String {
            return Err(KuError::runtime(
                format!("mysql.client config field '{name}' must be str"),
                span,
            ));
        }
    }
    for name in [
        "port",
        "max_connections",
        "max_waiters",
        "connect_timeout_ms",
        "acquire_timeout_ms",
        "query_timeout_ms",
    ] {
        if let Some(actual) = fields.get(name) {
            if *actual != Type::Int {
                return Err(KuError::runtime(
                    format!("mysql.client config field '{name}' must be int"),
                    span,
                ));
            }
        }
    }
    Ok(())
}

/// Fixed HTTP config shapes. Dynamic objects are checked again by the runtime;
/// these lists keep object-literal diagnostics aligned with those readers.
const HTTP_CLIENT_CONFIG_FIELDS: [&str; 3] =
    ["timeout_ms", "max_body_bytes", "max_idle_connections"];
const HTTP_REQUEST_CONFIG_FIELDS: [&str; 6] = [
    "method",
    "url",
    "headers",
    "body",
    "timeout_ms",
    "max_body_bytes",
];
const HTTP_SERVICE_CONFIG_FIELDS: [&str; 10] = [
    "read_header_timeout_ms",
    "read_body_timeout_ms",
    "write_timeout_ms",
    "idle_timeout_ms",
    "handler_timeout_ms",
    "max_body_bytes",
    "max_header_bytes",
    "max_connections",
    "max_active_requests",
    "max_pending_requests",
];

/// Reject a statically-known HTTP config field that the selected API does not
/// understand. `DynamicObject`/`Unknown` are validated by the interpreter/native
/// reader after their keys become available.
fn validate_http_config_fields(config_type: &Type, allowed: &[&str], span: Span) -> KuResult<()> {
    let Type::Object(fields) = config_type else {
        return Ok(());
    };
    let mut unknown: Vec<&String> = fields
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .collect();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Err(KuError::runtime(
            format!("unknown http config field '{key}'"),
            span,
        ));
    }
    Ok(())
}

fn http_service_type() -> Type {
    Type::Object(HashMap::from([
        ("kind".to_string(), Type::String),
        ("read_header_timeout_ms".to_string(), Type::Int),
        ("read_body_timeout_ms".to_string(), Type::Int),
        ("write_timeout_ms".to_string(), Type::Int),
        ("idle_timeout_ms".to_string(), Type::Int),
        ("handler_timeout_ms".to_string(), Type::Int),
        ("max_body_bytes".to_string(), Type::Int),
        ("max_header_bytes".to_string(), Type::Int),
        ("max_connections".to_string(), Type::Int),
        ("max_active_requests".to_string(), Type::Int),
        ("max_pending_requests".to_string(), Type::Int),
        (
            "routes".to_string(),
            Type::Array(Box::new(http_route_type())),
        ),
    ]))
}

fn http_route_type() -> Type {
    Type::Object(HashMap::from([
        ("method".to_string(), Type::String),
        ("path".to_string(), Type::String),
        (
            "param_names".to_string(),
            Type::Array(Box::new(Type::String)),
        ),
        ("handler".to_string(), Type::Unknown),
    ]))
}

fn http_status_type() -> Type {
    Type::Object(HashMap::from([
        ("ok".to_string(), Type::Int),
        ("created".to_string(), Type::Int),
        ("accepted".to_string(), Type::Int),
        ("noContent".to_string(), Type::Int),
        ("movedPermanently".to_string(), Type::Int),
        ("found".to_string(), Type::Int),
        ("seeOther".to_string(), Type::Int),
        ("notModified".to_string(), Type::Int),
        ("temporaryRedirect".to_string(), Type::Int),
        ("permanentRedirect".to_string(), Type::Int),
        ("badRequest".to_string(), Type::Int),
        ("unauthorized".to_string(), Type::Int),
        ("forbidden".to_string(), Type::Int),
        ("notFound".to_string(), Type::Int),
        ("methodNotAllowed".to_string(), Type::Int),
        ("notAcceptable".to_string(), Type::Int),
        ("requestTimeout".to_string(), Type::Int),
        ("conflict".to_string(), Type::Int),
        ("gone".to_string(), Type::Int),
        ("contentTooLarge".to_string(), Type::Int),
        ("uriTooLong".to_string(), Type::Int),
        ("unsupportedMedia".to_string(), Type::Int),
        ("rangeNotSatisfiable".to_string(), Type::Int),
        ("unprocessable".to_string(), Type::Int),
        ("tooManyRequests".to_string(), Type::Int),
        ("headerTooLarge".to_string(), Type::Int),
        ("internalError".to_string(), Type::Int),
        ("notImplemented".to_string(), Type::Int),
        ("badGateway".to_string(), Type::Int),
        ("serviceUnavailable".to_string(), Type::Int),
        ("gatewayTimeout".to_string(), Type::Int),
    ]))
}

fn http_code_type() -> Type {
    Type::Object(HashMap::from([
        ("SUCCESS".to_string(), Type::Int),
        ("CREATED".to_string(), Type::Int),
        ("ACCEPTED".to_string(), Type::Int),
        ("NO_CONTENT".to_string(), Type::Int),
        ("BAD_REQUEST".to_string(), Type::Int),
        ("UNAUTHORIZED".to_string(), Type::Int),
        ("FORBIDDEN".to_string(), Type::Int),
        ("NOT_FOUND".to_string(), Type::Int),
        ("VALIDATION_FAILED".to_string(), Type::Int),
        ("INTERNAL_ERROR".to_string(), Type::Int),
    ]))
}

fn std_module_object_type(module: &str, span: Span) -> KuResult<Type> {
    match module {
        "http" => Ok(Type::Object(HashMap::from([
            ("status".to_string(), http_status_type()),
            ("code".to_string(), http_code_type()),
        ]))),
        _ => Err(KuError::runtime(
            format!(
                "std module '{module}' cannot be used as an object value yet; access functions with '{module}.name(...)'"
            ),
            span,
        )),
    }
}

fn http_request_type() -> Type {
    Type::Object(HashMap::from([
        ("method".to_string(), Type::String),
        ("path".to_string(), Type::String),
        ("params".to_string(), Type::StringMap),
        ("query".to_string(), Type::StringMap),
        ("headers".to_string(), Type::StringMap),
        ("body".to_string(), Type::String),
    ]))
}

fn http_response_type() -> Type {
    Type::Object(HashMap::from([
        ("status".to_string(), Type::Int),
        ("headers".to_string(), Type::Object(HashMap::new())),
        ("body".to_string(), Type::String),
    ]))
}

fn http_listener_type() -> Type {
    Type::Object(HashMap::from([
        ("kind".to_string(), Type::String),
        ("listener_id".to_string(), Type::Int),
        ("address".to_string(), Type::String),
        ("service".to_string(), http_service_type()),
        ("compiled_router".to_string(), Type::Object(HashMap::new())),
    ]))
}

fn is_http_listener_type(ty: &Type) -> bool {
    let Type::Object(fields) = ty else {
        return false;
    };
    matches!(fields.get("kind"), Some(Type::String))
        && fields.contains_key("listener_id")
        && fields.contains_key("address")
        && fields.contains_key("service")
        && fields.contains_key("compiled_router")
}

fn is_http_service_type(ty: &Type) -> bool {
    let Type::Object(fields) = ty else {
        return false;
    };
    matches!(fields.get("kind"), Some(Type::String))
        && fields.contains_key("routes")
        && fields.contains_key("max_active_requests")
        && fields.contains_key("max_body_bytes")
}

fn assign_target_root_name(target: &AssignTarget) -> Option<&str> {
    match target {
        AssignTarget::Variable(name) => Some(name),
        AssignTarget::Index { target, .. } | AssignTarget::Field { target, .. } => {
            expr_root_name(target)
        }
    }
}

fn expr_root_name(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name),
        ExprKind::Index { target, .. } | ExprKind::Field { target, .. } => expr_root_name(target),
        _ => None,
    }
}

/// Reject the directly visible strong-reference cycles that local RC cannot
/// collect: assigning a closure (or a closure nested in an array/object value)
/// back into the same captured binding. Named local functions have a dedicated
/// self-recursion lowering that threads `__env` without capturing themselves, so
/// the diagnostic points recursive code to that cycle-free form.
fn reject_direct_closure_cycle(target: &str, value: &Expr, span: Span) -> KuResult<()> {
    if expression_contains_closure_capturing(value, target) {
        return Err(KuError::runtime(
            format!(
                "cannot store a closure that captures '{target}' back into '{target}': this would create a reference cycle; use a named local function for recursion"
            ),
            span,
        ));
    }
    Ok(())
}

fn expression_contains_closure_capturing(expr: &Expr, target: &str) -> bool {
    match &expr.kind {
        ExprKind::Function { params, body, .. } => {
            crate::runtime::interpreter::closure_capture_names(params, body).contains(target)
        }
        ExprKind::TryUnwrap { expr } => expression_contains_closure_capturing(expr, target),
        ExprKind::Array(values) => values
            .iter()
            .any(|value| expression_contains_closure_capturing(value, target)),
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => fields
            .iter()
            .any(|(_, value)| expression_contains_closure_capturing(value, target)),
        ExprKind::Match { arms, .. } => arms
            .iter()
            .any(|arm| expression_contains_closure_capturing(&arm.value, target)),
        // Calls and projections do not structurally preserve their input. A
        // closure passed to a function may be consumed and dropped before the
        // returned value is assigned, so recursively rejecting call arguments
        // would reject acyclic code. Alias-mediated cycles require a fuller
        // ownership graph and are intentionally outside this direct containment.
        ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Call { .. }
        | ExprKind::Index { .. }
        | ExprKind::Field { .. }
        | ExprKind::OptionalField { .. }
        | ExprKind::Await(_)
        | ExprKind::Variable(_)
        | ExprKind::Literal(_) => false,
    }
}

struct TemplateInterpolation {
    source: String,
    span: Span,
}

fn template_interpolations(raw: &str, span: Span) -> KuResult<Vec<TemplateInterpolation>> {
    let mut expressions = Vec::new();
    let mut chars = raw.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '\\' {
            chars.next();
            continue;
        }
        if ch != '{' {
            continue;
        }

        let expr_start = index + ch.len_utf8();
        let mut expr_source = String::new();
        let mut expr_end = expr_start;
        let mut found_end = false;
        while let Some((inner_index, inner)) = chars.next() {
            if inner == '\\' {
                if let Some((next_index, next)) = chars.next() {
                    expr_source.push('\\');
                    expr_source.push(next);
                    expr_end = next_index + next.len_utf8();
                }
                continue;
            }
            if inner == '}' {
                expr_end = inner_index;
                found_end = true;
                break;
            }
            expr_source.push(inner);
            expr_end = inner_index + inner.len_utf8();
        }
        if !found_end {
            return Err(KuError::runtime(
                "unterminated template interpolation",
                span,
            ));
        }
        if expr_source.trim().is_empty() {
            return Err(KuError::runtime("empty template interpolation", span));
        }
        let content_start = advance_position(span.start, "`");
        let start = advance_position(content_start, &raw[..expr_start]);
        let end = advance_position(content_start, &raw[..expr_end]);
        expressions.push(TemplateInterpolation {
            source: expr_source,
            span: Span::new(start, end),
        });
    }
    Ok(expressions)
}

fn map_template_error(err: KuError, interpolation: &TemplateInterpolation) -> KuError {
    if err.span == Span::default() {
        return err;
    }
    KuError::new(
        err.kind,
        err.message,
        Span::new(
            advance_position(
                interpolation.span.start,
                prefix_by_offset(&interpolation.source, err.span.start.offset),
            ),
            advance_position(
                interpolation.span.start,
                prefix_by_offset(&interpolation.source, err.span.end.offset),
            ),
        ),
    )
}

fn prefix_by_offset(source: &str, offset: usize) -> &str {
    let mut end = offset.min(source.len());
    while end > 0 && !source.is_char_boundary(end) {
        end -= 1;
    }
    &source[..end]
}

fn advance_position(mut position: crate::span::Position, text: &str) -> crate::span::Position {
    for ch in text.chars() {
        position.offset += ch.len_utf8();
        if ch == '\n' {
            position.line += 1;
            position.column = 1;
        } else {
            position.column += 1;
        }
    }
    position
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_resource_detection_is_recursive_and_cycle_safe() {
        let mut checker = Checker::new();
        checker.structs.insert(
            "Recursive".to_string(),
            StructType {
                fields: HashMap::from([(
                    "next".to_string(),
                    Type::Struct("Recursive".to_string()),
                )]),
            },
        );
        assert!(!checker.type_contains_native_resource(&Type::Struct("Recursive".into())));

        checker
            .structs
            .get_mut("Recursive")
            .expect("recursive layout")
            .fields
            .insert(
                "connection".to_string(),
                Type::Native(metadata::PG_RESULT.to_string()),
            );
        assert!(checker.type_contains_native_resource(&Type::Struct("Recursive".into())));

        checker.enums.insert(
            "RecursiveEnum".to_string(),
            EnumType {
                variants: HashMap::from([
                    (
                        "Next".to_string(),
                        vec![Type::Enum("RecursiveEnum".to_string())],
                    ),
                    (
                        "Connection".to_string(),
                        vec![Type::Native(metadata::MYSQL_CLIENT.to_string())],
                    ),
                ]),
            },
        );
        assert!(checker.type_contains_native_resource(&Type::Enum("RecursiveEnum".into())));

        let nested = Type::Union(vec![
            Type::Int,
            Type::Result(Box::new(Type::Array(Box::new(Type::Native(
                metadata::REDIS_CLIENT.to_string(),
            ))))),
        ]);
        assert!(checker.type_contains_native_resource(&nested));
    }
}

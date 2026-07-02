use std::{
    collections::{HashMap, HashSet},
    fmt,
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
    Unknown,
    Void,
}

#[derive(Debug, Clone)]
struct FunctionSig {
    id: FunctionId,
    params: Vec<IrType>,
    returns: IrType,
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
            let mut lower = FunctionLowerer::new(&signatures, &layouts, signature.returns.clone());
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
            });
        }
    }
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
        IrInst::BeginTry { .. }
        | IrInst::EndTry
        | IrInst::BindError { .. }
        | IrInst::DefineClosure { .. }
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
        IrExprKind::Literal(_) | IrExprKind::Local(_) | IrExprKind::Temp(_) => expr,
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
    next_local_function_id: usize,
    try_handlers: Vec<IrTryHandler>,
    pattern_bindings: HashMap<String, IrExpr>,
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
            next_local_function_id: 10_000,
            try_handlers: Vec::new(),
            pattern_bindings: HashMap::new(),
        }
    }

    fn lower_block_body(&mut self, name: &str, body: &[Stmt], span: Span) -> KuResult<()> {
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
                let value = self.lower_expr(value)?;
                let ty = ty
                    .as_ref()
                    .map(|ty| lower_type(ty, self.layouts))
                    .unwrap_or_else(|| value.ty.clone());
                self.locals.insert(name.clone(), ty.clone());
                self.current.instructions.push(IrInst::Let {
                    name: name.clone(),
                    ty,
                    value,
                });
            }
            Stmt::Assign { name, value, .. } => {
                let value = self.lower_expr(value)?;
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
            Stmt::Function(function) => {
                let id = FunctionId(self.next_local_function_id);
                self.next_local_function_id += 1;
                self.locals.insert(function.name.clone(), IrType::Function);
                self.current.instructions.push(IrInst::DefineClosure {
                    name: function.name.clone(),
                    function_id: id,
                    captures: captured_names(function),
                });
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                ..
            } => self.lower_try(body, catch_name, catch_body, finally_body)?,
            Stmt::Fail { value, .. } => {
                let value = self.lower_expr(value)?;
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
                let value = value
                    .as_ref()
                    .map(|value| self.lower_expr(value))
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

    fn field_type(&self, target: &IrType, field_name: &str) -> IrType {
        let IrType::Named(struct_name) = target else {
            return IrType::Unknown;
        };
        if struct_name == "__ku_error_type" && matches!(field_name, "domain" | "code" | "message") {
            return IrType::Str;
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
            ExprKind::Literal(literal) => Ok(IrExpr {
                kind: IrExprKind::Literal(literal_text(literal)),
                ty: literal_type(literal),
            }),
            ExprKind::Variable(name) => {
                if let Some(value) = self.pattern_bindings.get(name) {
                    return Ok(value.clone());
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
                if let ExprKind::Field { target, name } = &callee.kind {
                    if name == "clone" && args.is_empty() {
                        let target = self.lower_expr(target)?;
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
                let lowered_args = args
                    .iter()
                    .map(|arg| self.lower_expr(arg))
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
                let (kind, ty) = call_kind_and_type(callee, &lowered_args, self.signatures);
                let callee = self.lower_expr(callee)?;
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
                let target = self.lower_expr(target)?;
                let ty = self.field_type(&target.ty, name);
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(target),
                        name: name.clone(),
                    },
                    ty,
                })
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
                let fields = fields
                    .iter()
                    .map(|(field, value)| Ok((field.clone(), self.lower_expr(value)?)))
                    .collect::<KuResult<Vec<_>>>()?;
                Ok(IrExpr {
                    kind: IrExprKind::StructLiteral {
                        name: name.clone(),
                        fields,
                    },
                    ty: IrType::Named(name.clone()),
                })
            }
            ExprKind::ObjectLiteral { .. } => Ok(unsupported_expr("object literal")),
            ExprKind::Match { value, arms } => self.lower_match(value, arms, expr.span),
            ExprKind::Function { .. } => Ok(IrExpr {
                kind: IrExprKind::Literal("<closure>".to_string()),
                ty: IrType::Function,
            }),
        }
    }

    fn emit_temp(&mut self, value: IrExpr) -> KuResult<IrExpr> {
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
            self.current.instructions.push(IrInst::Store {
                target: IrLValue::Local(result_name.clone()),
                value: arm_value,
            });
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
        if let Some(handler) = self.try_handlers.last().cloned() {
            let error_name = handler.error_name;
            if self.locals.contains_key(&error_name) {
                self.current.instructions.push(IrInst::Store {
                    target: IrLValue::Local(error_name.clone()),
                    value: result.clone(),
                });
            } else {
                self.locals.insert(error_name.clone(), result.ty.clone());
                self.current.instructions.push(IrInst::Let {
                    name: error_name,
                    ty: result.ty.clone(),
                    value: result.clone(),
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

fn signature_return_type(signature: &Signature, args: &[IrExpr]) -> IrType {
    pattern_to_ir_type(&signature.returns, args).unwrap_or(IrType::Unknown)
}

fn pattern_to_ir_type(pattern: &TypePattern, args: &[IrExpr]) -> Option<IrType> {
    match pattern {
        TypePattern::Int => Some(IrType::Int),
        TypePattern::Bool => Some(IrType::Bool),
        TypePattern::String => Some(IrType::Str),
        TypePattern::Null
        | TypePattern::Unknown
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

fn captured_names(function: &crate::ast::FnDecl) -> Vec<String> {
    let mut bound = HashSet::new();
    bound.insert(function.name.clone());
    for param in &function.params {
        bound.insert(param.name.clone());
    }
    let mut free = HashSet::new();
    for stmt in &function.body {
        collect_free_stmt_names(stmt, &mut bound, &mut free);
    }
    let mut names = free.into_iter().collect::<Vec<_>>();
    names.sort();
    names
}

fn collect_free_stmt_names(stmt: &Stmt, bound: &mut HashSet<String>, free: &mut HashSet<String>) {
    match stmt {
        Stmt::VarDecl { name, value, .. } | Stmt::Assign { name, value, .. } => {
            collect_free_expr_names(value, bound, free);
            bound.insert(name.clone());
        }
        Stmt::Print { value, .. } => collect_free_expr_names(value, bound, free),
        Stmt::AssignTarget { target, value, .. } => {
            collect_free_lvalue_names(target, bound, free);
            collect_free_expr_names(value, bound, free);
        }
        Stmt::CompoundAssign { target, value, .. } => {
            collect_free_lvalue_names(target, bound, free);
            collect_free_expr_names(value, bound, free);
        }
        Stmt::DestructureAssign { names, values, .. } => {
            for value in values {
                collect_free_expr_names(value, bound, free);
            }
            for name in names.iter().flatten() {
                bound.insert(name.clone());
            }
        }
        Stmt::ObjectDestructureAssign {
            bindings,
            rest,
            value,
            ..
        } => {
            collect_free_expr_names(value, bound, free);
            for binding in bindings {
                if let Some(default) = &binding.default {
                    collect_free_expr_names(default, bound, free);
                }
                if let Some(local) = &binding.local {
                    bound.insert(local.clone());
                }
            }
            if let Some(local) = rest.as_ref().and_then(|rest| rest.local.as_ref()) {
                bound.insert(local.clone());
            }
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_free_expr_names(condition, bound, free);
            collect_free_nested_block(then_branch, bound, free);
            collect_free_nested_block(else_branch, bound, free);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_free_expr_names(condition, bound, free);
            collect_free_nested_block(body, bound, free);
        }
        Stmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            collect_free_expr_names(iterable, bound, free);
            let mut scoped = bound.clone();
            scoped.insert(name.clone());
            collect_free_nested_block(body, &mut scoped, free);
        }
        Stmt::Function(function) => {
            bound.insert(function.name.clone());
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            finally_body,
            ..
        } => {
            collect_free_nested_block(body, bound, free);
            let mut catch_bound = bound.clone();
            if let Some(name) = catch_name {
                catch_bound.insert(name.clone());
            }
            collect_free_nested_block(catch_body, &mut catch_bound, free);
            collect_free_nested_block(finally_body, bound, free);
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } => {
            collect_free_expr_names(value, bound, free)
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_free_expr_names(value, bound, free);
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Expr { expr, .. } => collect_free_expr_names(expr, bound, free),
    }
}

fn collect_free_nested_block(
    body: &[Stmt],
    bound: &mut HashSet<String>,
    free: &mut HashSet<String>,
) {
    for stmt in body {
        collect_free_stmt_names(stmt, bound, free);
    }
}

fn collect_free_lvalue_names(
    target: &AssignTarget,
    bound: &HashSet<String>,
    free: &mut HashSet<String>,
) {
    match target {
        AssignTarget::Variable(name) => {
            if !bound.contains(name) {
                free.insert(name.clone());
            }
        }
        AssignTarget::Index { target, index } => {
            collect_free_expr_names(target, bound, free);
            collect_free_expr_names(index, bound, free);
        }
        AssignTarget::Field { target, .. } => collect_free_expr_names(target, bound, free),
    }
}

fn collect_free_expr_names(expr: &Expr, bound: &HashSet<String>, free: &mut HashSet<String>) {
    match &expr.kind {
        ExprKind::Variable(name) => {
            if !bound.contains(name) {
                free.insert(name.clone());
            }
        }
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => {
            collect_free_expr_names(expr, bound, free)
        }
        ExprKind::Binary { left, right, .. } => {
            collect_free_expr_names(left, bound, free);
            collect_free_expr_names(right, bound, free);
        }
        ExprKind::Call { callee, args } => {
            collect_free_expr_names(callee, bound, free);
            for arg in args {
                collect_free_expr_names(arg, bound, free);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_free_expr_names(value, bound, free);
            }
        }
        ExprKind::Index { target, index } => {
            collect_free_expr_names(target, bound, free);
            collect_free_expr_names(index, bound, free);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_free_expr_names(target, bound, free)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_free_expr_names(value, bound, free);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_free_expr_names(value, bound, free);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_free_expr_names(guard, bound, free);
                }
                collect_free_expr_names(&arm.value, bound, free);
            }
        }
        ExprKind::Function { params, body, .. } => {
            let mut nested = bound.clone();
            for param in params {
                nested.insert(param.name.clone());
            }
            collect_free_nested_block(body, &mut nested, free);
        }
        ExprKind::Literal(_) => {}
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

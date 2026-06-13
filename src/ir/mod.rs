use std::{collections::HashMap, fmt};

use crate::{
    ast::{
        AssignTarget, BinaryOp, Expr, ExprKind, Item, Literal, Program, Stmt, TypeName, UnaryOp,
    },
    error::{KuError, KuResult},
    span::Span,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrProgram {
    pub functions: Vec<IrFunction>,
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
    Let {
        name: String,
        ty: IrType,
        value: IrExpr,
    },
    Store {
        target: IrLValue,
        value: IrExpr,
    },
    Expr(IrExpr),
    Fail(IrExpr),
    Panic(IrExpr),
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
                    params: function.params.iter().map(|p| lower_type(&p.ty)).collect(),
                    returns: function
                        .return_type
                        .as_ref()
                        .map(lower_type)
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
            let mut lower = FunctionLowerer::new(&signatures);
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
    Ok(IrProgram { functions })
}

impl fmt::Display for IrProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            IrType::Named(name) => write!(f, "{name}"),
            IrType::Function => write!(f, "function"),
            IrType::Unknown => write!(f, "unknown"),
            IrType::Void => write!(f, "void"),
        }
    }
}

impl fmt::Display for IrInst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrInst::Let { name, ty, value } => write!(f, "let {name}: {ty} = {value}"),
            IrInst::Store { target, value } => write!(f, "store {target} = {value}"),
            IrInst::Expr(value) => write!(f, "expr {value}"),
            IrInst::Fail(value) => write!(f, "fail {value}"),
            IrInst::Panic(value) => write!(f, "panic {value}"),
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
    locals: HashMap<String, IrType>,
    blocks: Vec<IrBlock>,
    current: IrBlock,
    next_block_id: usize,
    next_local_function_id: usize,
}

impl<'a> FunctionLowerer<'a> {
    fn new(signatures: &'a HashMap<String, FunctionSig>) -> Self {
        Self {
            signatures,
            locals: HashMap::new(),
            blocks: Vec::new(),
            current: IrBlock {
                id: BlockId(0),
                name: "entry".to_string(),
                instructions: Vec::new(),
                terminator: IrTerminator::Next,
            },
            next_block_id: 1,
            next_local_function_id: 10_000,
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
                    .map(lower_type)
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
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => self.lower_if(condition, then_branch, else_branch)?,
            Stmt::While {
                condition, body, ..
            } => self.lower_while(condition, body)?,
            Stmt::For { name, iterable, .. } => {
                let iterable = self.lower_expr(iterable)?;
                self.current.instructions.push(IrInst::Unsupported {
                    reason: format!("for {name} in {iterable}"),
                });
            }
            Stmt::Function(function) => {
                let id = FunctionId(self.next_local_function_id);
                self.next_local_function_id += 1;
                self.locals.insert(function.name.clone(), IrType::Function);
                self.current.instructions.push(IrInst::DefineClosure {
                    name: function.name.clone(),
                    function_id: id,
                    captures: captured_names(&function.body),
                });
            }
            Stmt::Try { .. } => self.current.instructions.push(IrInst::Unsupported {
                reason: "try/catch/finally lowering is pending".to_string(),
            }),
            Stmt::Fail { value, .. } => {
                let value = self.lower_expr(value)?;
                self.current.instructions.push(IrInst::Fail(value));
                self.current.terminator = IrTerminator::Unreachable;
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
                self.current.terminator = IrTerminator::Return(value);
            }
            Stmt::Print { value, .. } | Stmt::Expr { expr: value, .. } => {
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

    fn lower_lvalue(&mut self, target: &AssignTarget) -> KuResult<IrLValue> {
        match target {
            AssignTarget::Variable(name) => Ok(IrLValue::Local(name.clone())),
            AssignTarget::Index { target, index } => Ok(IrLValue::Index {
                target: self.lower_expr(target)?,
                index: self.lower_expr(index)?,
            }),
            AssignTarget::Field { target, name } => Ok(IrLValue::Field {
                target: self.lower_expr(target)?,
                name: name.clone(),
            }),
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> KuResult<IrExpr> {
        match &expr.kind {
            ExprKind::Literal(literal) => Ok(IrExpr {
                kind: IrExprKind::Literal(literal_text(literal)),
                ty: literal_type(literal),
            }),
            ExprKind::Variable(name) => Ok(IrExpr {
                kind: IrExprKind::Local(name.clone()),
                ty: self.locals.get(name).cloned().unwrap_or(IrType::Unknown),
            }),
            ExprKind::Unary { op, expr } => {
                let expr = self.lower_expr(expr)?;
                let ty = match op {
                    UnaryOp::Negate => expr.ty.clone(),
                    UnaryOp::Not => IrType::Bool,
                };
                Ok(IrExpr {
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
                Ok(IrExpr {
                    kind: IrExprKind::Binary {
                        left: Box::new(left),
                        op: *op,
                        right: Box::new(right),
                    },
                    ty,
                })
            }
            ExprKind::Call { callee, args } => {
                let lowered_args = args
                    .iter()
                    .map(|arg| self.lower_expr(arg))
                    .collect::<KuResult<Vec<_>>>()?;
                let (kind, ty) = call_kind_and_type(callee, self.signatures);
                Ok(IrExpr {
                    kind: IrExprKind::Call {
                        callee: Box::new(self.lower_expr(callee)?),
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
                Ok(IrExpr {
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
                Ok(IrExpr {
                    kind: IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    ty,
                })
            }
            ExprKind::Field { target, name } => Ok(IrExpr {
                kind: IrExprKind::Field {
                    target: Box::new(self.lower_expr(target)?),
                    name: name.clone(),
                },
                ty: IrType::Unknown,
            }),
            ExprKind::TryUnwrap { expr } => {
                let expr = self.lower_expr(expr)?;
                let ty = match &expr.ty {
                    IrType::Result(inner) => *inner.clone(),
                    _ => IrType::Unknown,
                };
                Ok(IrExpr {
                    kind: IrExprKind::TryUnwrap(Box::new(expr)),
                    ty,
                })
            }
            ExprKind::StructLiteral { name, .. } => Ok(unsupported_expr(format!("{name} literal"))),
            ExprKind::ObjectLiteral { .. } => Ok(unsupported_expr("object literal")),
            ExprKind::Match { .. } => Ok(unsupported_expr("match expression")),
            ExprKind::Function { .. } => Ok(IrExpr {
                kind: IrExprKind::Literal("<closure>".to_string()),
                ty: IrType::Function,
            }),
        }
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

fn lower_type(ty: &TypeName) -> IrType {
    match ty {
        TypeName::Int => IrType::Int,
        TypeName::Float => IrType::Float,
        TypeName::Bool => IrType::Bool,
        TypeName::String => IrType::Str,
        TypeName::Null => IrType::Null,
        TypeName::Array(inner) => IrType::Array(Box::new(lower_type(inner))),
        TypeName::Result(inner) => IrType::Result(Box::new(lower_type(inner))),
        TypeName::Custom(name) => IrType::Named(name.clone()),
    }
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
    signatures: &HashMap<String, FunctionSig>,
) -> (IrCallKind, IrType) {
    if let ExprKind::Variable(name) = &callee.kind {
        if let Some(signature) = signatures.get(name) {
            return (IrCallKind::Direct(signature.id), signature.returns.clone());
        }
        if matches!(name.as_str(), "print" | "len" | "str" | "ok" | "err") {
            return (
                IrCallKind::Intrinsic(name.clone()),
                if name == "ok" || name == "err" {
                    IrType::Result(Box::new(IrType::Unknown))
                } else {
                    IrType::Unknown
                },
            );
        }
    }
    if let Some(name) = dotted_name(callee) {
        return (IrCallKind::Intrinsic(name), IrType::Unknown);
    }
    (IrCallKind::Indirect, IrType::Unknown)
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

fn captured_names(body: &[Stmt]) -> Vec<String> {
    let mut names = Vec::new();
    for stmt in body {
        collect_stmt_names(stmt, &mut names);
    }
    names.sort();
    names.dedup();
    names
}

fn collect_stmt_names(stmt: &Stmt, names: &mut Vec<String>) {
    match stmt {
        Stmt::VarDecl { value, .. } | Stmt::Assign { value, .. } | Stmt::Print { value, .. } => {
            collect_expr_names(value, names)
        }
        Stmt::AssignTarget { target, value, .. } => {
            collect_lvalue_names(target, names);
            collect_expr_names(value, names);
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_expr_names(condition, names);
            for stmt in then_branch.iter().chain(else_branch.iter()) {
                collect_stmt_names(stmt, names);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_expr_names(condition, names);
            for stmt in body {
                collect_stmt_names(stmt, names);
            }
        }
        Stmt::For { iterable, body, .. } => {
            collect_expr_names(iterable, names);
            for stmt in body {
                collect_stmt_names(stmt, names);
            }
        }
        Stmt::Function(function) => {
            for stmt in &function.body {
                collect_stmt_names(stmt, names);
            }
        }
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            for stmt in body
                .iter()
                .chain(catch_body.iter())
                .chain(finally_body.iter())
            {
                collect_stmt_names(stmt, names);
            }
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } => collect_expr_names(value, names),
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_expr_names(value, names);
            }
        }
        Stmt::Expr { expr, .. } => collect_expr_names(expr, names),
    }
}

fn collect_lvalue_names(target: &AssignTarget, names: &mut Vec<String>) {
    match target {
        AssignTarget::Variable(name) => names.push(name.clone()),
        AssignTarget::Index { target, index } => {
            collect_expr_names(target, names);
            collect_expr_names(index, names);
        }
        AssignTarget::Field { target, .. } => collect_expr_names(target, names),
    }
}

fn collect_expr_names(expr: &Expr, names: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Variable(name) => names.push(name.clone()),
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } => {
            collect_expr_names(expr, names)
        }
        ExprKind::Binary { left, right, .. } => {
            collect_expr_names(left, names);
            collect_expr_names(right, names);
        }
        ExprKind::Call { callee, args } => {
            collect_expr_names(callee, names);
            for arg in args {
                collect_expr_names(arg, names);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_expr_names(value, names);
            }
        }
        ExprKind::Index { target, index } => {
            collect_expr_names(target, names);
            collect_expr_names(index, names);
        }
        ExprKind::Field { target, .. } => collect_expr_names(target, names),
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_expr_names(value, names);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_expr_names(value, names);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_expr_names(guard, names);
                }
                collect_expr_names(&arm.value, names);
            }
        }
        ExprKind::Function { body, .. } => {
            for stmt in body {
                collect_stmt_names(stmt, names);
            }
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

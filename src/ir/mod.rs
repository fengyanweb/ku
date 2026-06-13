use std::fmt;

use crate::{
    ast::{BinaryOp, Expr, ExprKind, Item, Literal, Program, Stmt, TypeName, UnaryOp},
    error::{KuError, KuResult},
    span::Span,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrProgram {
    pub functions: Vec<IrFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrFunction {
    pub name: String,
    pub params: Vec<IrParam>,
    pub return_type: IrType,
    pub blocks: Vec<IrBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrParam {
    pub name: String,
    pub ty: IrType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrBlock {
    pub name: String,
    pub instructions: Vec<IrInst>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrInst {
    Let { name: String, value: String },
    Assign { name: String, value: String },
    Expr(String),
    Return(Option<String>),
    Branch { condition: String },
    Loop { condition: String },
    For { name: String, iterable: String },
    Try,
    Fail(String),
    Panic(String),
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
    Void,
}

pub fn lower_program(program: &Program) -> KuResult<IrProgram> {
    let functions = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Function(function) => Some(function),
            _ => None,
        })
        .map(|function| {
            let params = function
                .params
                .iter()
                .map(|param| IrParam {
                    name: param.name.clone(),
                    ty: lower_type(&param.ty),
                })
                .collect();
            let mut lower = FunctionLowerer::new();
            lower.lower_block("entry", &function.body, function.span)?;
            Ok(IrFunction {
                name: function.name.clone(),
                params,
                return_type: function
                    .return_type
                    .as_ref()
                    .map(lower_type)
                    .unwrap_or(IrType::Void),
                blocks: lower.blocks,
            })
        })
        .collect::<KuResult<Vec<_>>>()?;
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
            IrType::Void => write!(f, "void"),
        }
    }
}

impl fmt::Display for IrInst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrInst::Let { name, value } => write!(f, "let {name} = {value}"),
            IrInst::Assign { name, value } => write!(f, "assign {name} = {value}"),
            IrInst::Expr(value) => write!(f, "expr {value}"),
            IrInst::Return(Some(value)) => write!(f, "return {value}"),
            IrInst::Return(None) => write!(f, "return"),
            IrInst::Branch { condition } => write!(f, "branch {condition}"),
            IrInst::Loop { condition } => write!(f, "loop {condition}"),
            IrInst::For { name, iterable } => write!(f, "for {name} in {iterable}"),
            IrInst::Try => write!(f, "try"),
            IrInst::Fail(value) => write!(f, "fail {value}"),
            IrInst::Panic(value) => write!(f, "panic {value}"),
        }
    }
}

struct FunctionLowerer {
    blocks: Vec<IrBlock>,
}

impl FunctionLowerer {
    fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    fn lower_block(&mut self, name: &str, body: &[Stmt], span: Span) -> KuResult<()> {
        let mut block = IrBlock {
            name: name.to_string(),
            instructions: Vec::new(),
        };
        for stmt in body {
            block.instructions.push(lower_stmt(stmt)?);
        }
        if block.instructions.len() > 10_000 {
            return Err(KuError::runtime("ir block is too large", span));
        }
        self.blocks.push(block);
        Ok(())
    }
}

fn lower_stmt(stmt: &Stmt) -> KuResult<IrInst> {
    Ok(match stmt {
        Stmt::VarDecl { name, value, .. } => IrInst::Let {
            name: name.clone(),
            value: expr_text(value),
        },
        Stmt::Assign { name, value, .. } => IrInst::Assign {
            name: name.clone(),
            value: expr_text(value),
        },
        Stmt::AssignTarget { target, value, .. } => IrInst::Assign {
            name: format!("{target:?}"),
            value: expr_text(value),
        },
        Stmt::If { condition, .. } => IrInst::Branch {
            condition: expr_text(condition),
        },
        Stmt::While { condition, .. } => IrInst::Loop {
            condition: expr_text(condition),
        },
        Stmt::For { name, iterable, .. } => IrInst::For {
            name: name.clone(),
            iterable: expr_text(iterable),
        },
        Stmt::Function(function) => IrInst::Expr(format!("local fn {}", function.name)),
        Stmt::Try { .. } => IrInst::Try,
        Stmt::Fail { value, .. } => IrInst::Fail(expr_text(value)),
        Stmt::Panic { value, .. } => IrInst::Panic(expr_text(value)),
        Stmt::Return { value, .. } => IrInst::Return(value.as_ref().map(expr_text)),
        Stmt::Print { value, .. } | Stmt::Expr { expr: value, .. } => {
            IrInst::Expr(expr_text(value))
        }
    })
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

fn expr_text(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::Literal(Literal::Int(value)) => value.to_string(),
        ExprKind::Literal(Literal::Float(value)) => value.to_string(),
        ExprKind::Literal(Literal::Bool(value)) => value.to_string(),
        ExprKind::Literal(Literal::String(value) | Literal::TemplateString(value)) => {
            format!("{value:?}")
        }
        ExprKind::Literal(Literal::Null) => "null".to_string(),
        ExprKind::Unary { op, expr } => format!("{}{}", unary_text(*op), expr_text(expr)),
        ExprKind::Binary { left, op, right } => {
            format!(
                "{} {} {}",
                expr_text(left),
                binary_text(*op),
                expr_text(right)
            )
        }
        ExprKind::Call { callee, args } => {
            let args = args.iter().map(expr_text).collect::<Vec<_>>().join(", ");
            format!("{}({args})", expr_text(callee))
        }
        ExprKind::Array(values) => {
            let values = values.iter().map(expr_text).collect::<Vec<_>>().join(", ");
            format!("[{values}]")
        }
        ExprKind::Index { target, index } => format!("{}[{}]", expr_text(target), expr_text(index)),
        ExprKind::Field { target, name } => format!("{}.{}", expr_text(target), name),
        ExprKind::TryUnwrap { expr } => format!("{}?", expr_text(expr)),
        ExprKind::StructLiteral { name, .. } => format!("{name} {{ ... }}"),
        ExprKind::ObjectLiteral { .. } => "{ ... }".to_string(),
        ExprKind::Match { .. } => "match ...".to_string(),
        ExprKind::Function { .. } => "fn (...) { ... }".to_string(),
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

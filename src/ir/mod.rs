use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use crate::{
    ast::{
        AssignTarget, BinaryOp, EnumDecl, Expr, ExprKind, Item, Literal, Program, Stmt, StructDecl,
        TypeName, UnaryOp,
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
    Ok(IrProgram {
        functions,
        layouts: lower_layouts(program),
    })
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
            IrInst::BindError { name } => write!(f, "bind_error {name}"),
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
    next_temp_id: usize,
    next_local_function_id: usize,
    try_handlers: Vec<IrTryHandler>,
}

#[derive(Debug, Clone, Copy)]
struct IrTryHandler {
    catch_block: Option<BlockId>,
    finally_block: Option<BlockId>,
    after_block: BlockId,
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
            next_temp_id: 0,
            next_local_function_id: 10_000,
            try_handlers: Vec::new(),
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
            Stmt::For {
                name,
                iterable,
                body,
                ..
            } => {
                let iterable = self.lower_expr(iterable)?;
                self.lower_for(name, iterable, body)?;
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
        let after_id = self.next_block("try_after");
        self.current.instructions.push(IrInst::BeginTry {
            catch_block: catch_id,
            finally_block: finally_id,
            after_block: after_id,
        });
        self.try_handlers.push(IrTryHandler {
            catch_block: catch_id,
            finally_block: finally_id,
            after_block: after_id,
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
                .map(|name| self.locals.insert(name.clone(), IrType::Str));
            if let Some(name) = catch_name {
                self.current
                    .instructions
                    .push(IrInst::BindError { name: name.clone() });
            }
            for stmt in catch_body {
                self.lower_stmt(stmt)?;
                if self.current.terminator != IrTerminator::Next {
                    break;
                }
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
                let lowered_args = args
                    .iter()
                    .map(|arg| self.lower_expr(arg))
                    .collect::<KuResult<Vec<_>>>()?;
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
                let target = self.lower_expr(target)?;
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(target),
                        name: name.clone(),
                    },
                    ty: IrType::Unknown,
                })
            }
            ExprKind::TryUnwrap { expr } => {
                let expr = self.lower_expr(expr)?;
                let ty = match &expr.ty {
                    IrType::Result(inner) => *inner.clone(),
                    _ => IrType::Unknown,
                };
                self.emit_try_unwrap(expr, ty)
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

    fn emit_try_unwrap(&mut self, result: IrExpr, ty: IrType) -> KuResult<IrExpr> {
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

    fn err_terminator(&self, result: IrExpr) -> IrTerminator {
        if let Some(handler) = self.try_handlers.last() {
            IrTerminator::Jump(
                handler
                    .catch_block
                    .or(handler.finally_block)
                    .unwrap_or(handler.after_block),
            )
        } else {
            IrTerminator::PropagateErr(result)
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

fn lower_layouts(program: &Program) -> IrLayoutTable {
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    for item in &program.items {
        match item {
            Item::Struct(decl) => structs.push(lower_struct_layout(decl)),
            Item::Enum(decl) => enums.push(lower_enum_layout(decl)),
            _ => {}
        }
    }
    IrLayoutTable { structs, enums }
}

fn lower_struct_layout(decl: &StructDecl) -> IrStructLayout {
    IrStructLayout {
        name: decl.name.clone(),
        fields: decl
            .fields
            .iter()
            .enumerate()
            .map(|(offset, field)| IrFieldLayout {
                name: field.name.clone(),
                ty: lower_type(&field.ty),
                offset,
            })
            .collect(),
    }
}

fn lower_enum_layout(decl: &EnumDecl) -> IrEnumLayout {
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
                        ty: lower_type(&field.ty),
                        offset,
                    })
                    .collect(),
            })
            .collect(),
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
        TypePattern::Unknown | TypePattern::Any | TypePattern::StringOrStringArray => {
            Some(IrType::Unknown)
        }
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
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } => {
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
        ExprKind::Field { target, .. } => collect_free_expr_names(target, bound, free),
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

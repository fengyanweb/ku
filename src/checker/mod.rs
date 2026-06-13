use std::collections::{HashMap, HashSet};

use crate::{
    ast::*,
    error::{KuError, KuResult},
    span::Span,
};

const MAX_CHECK_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
enum Type {
    Int,
    Float,
    Bool,
    String,
    Null,
    Array(Box<Type>),
    Result(Box<Type>),
    Object(HashMap<String, Type>),
    Struct(String),
    Enum(String),
    Void,
    FunctionValue {
        params: Vec<FunctionValueParam>,
        return_type: Option<Box<Type>>,
        body: Vec<Stmt>,
    },
    Unknown,
}

#[derive(Debug, Clone)]
struct VarType {
    ty: Type,
    mutable: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct FunctionValueParam {
    name: String,
    ty: Option<Type>,
}

#[derive(Debug, Clone)]
struct FunctionType {
    params: Vec<Type>,
    returns: Type,
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
    template_mode: bool,
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
            template_mode: false,
        }
    }

    pub fn check(mut self, program: &Program) -> KuResult<()> {
        let mut top_level_names = HashSet::new();
        for item in &program.items {
            match item {
                Item::Function(function) => {
                    if !top_level_names.insert(function.name.clone()) {
                        return Err(KuError::runtime(
                            format!("top-level name '{}' is already defined", function.name),
                            function.span,
                        ));
                    }
                }
                Item::Import(_) => {}
                Item::Struct(decl) => {
                    if !top_level_names.insert(decl.name.clone()) {
                        return Err(KuError::runtime(
                            format!("top-level name '{}' is already defined", decl.name),
                            decl.span,
                        ));
                    }
                }
                Item::Enum(decl) => {
                    if !top_level_names.insert(decl.name.clone()) {
                        return Err(KuError::runtime(
                            format!("top-level name '{}' is already defined", decl.name),
                            decl.span,
                        ));
                    }
                }
                Item::Module(decl) => {
                    if !top_level_names.insert(decl.name.clone()) {
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
                self.functions.insert(
                    function.name.clone(),
                    FunctionType {
                        params: function
                            .params
                            .iter()
                            .map(|p| self.resolve_type_name(&p.ty, p.span))
                            .collect::<KuResult<Vec<_>>>()?,
                        returns: function
                            .return_type
                            .as_ref()
                            .map(|ty| self.resolve_type_name(ty, function.span))
                            .transpose()?
                            .unwrap_or(Type::Void),
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
                self.resolve_type_name(&field.ty, field.span)?,
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
                    .map(|p| self.resolve_type_name(&p.ty, p.span))
                    .collect::<KuResult<Vec<_>>>()?,
            );
        }
        self.enums.insert(decl.name.clone(), EnumType { variants });
        Ok(())
    }

    fn check_function(&mut self, function: &FnDecl) -> KuResult<()> {
        reject_duplicate_params(function)?;
        self.push_scope();
        self.current_return = function
            .return_type
            .as_ref()
            .map(|ty| self.resolve_type_name(ty, function.span))
            .transpose()?
            .unwrap_or(Type::Void);
        for param in &function.params {
            self.define(
                param.name.clone(),
                self.resolve_type_name(&param.ty, param.span)?,
                false,
                param.span,
            )?;
        }
        for stmt in &function.body {
            self.check_stmt(stmt)?;
        }
        if self.current_return != Type::Void && !block_may_return(&function.body) {
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
        Ok(())
    }

    fn resolve_type_name(&self, name: &TypeName, span: Span) -> KuResult<Type> {
        match name {
            TypeName::Int => Ok(Type::Int),
            TypeName::Float => Ok(Type::Float),
            TypeName::Bool => Ok(Type::Bool),
            TypeName::String => Ok(Type::String),
            TypeName::Null => Ok(Type::Null),
            TypeName::Array(inner) => {
                Ok(Type::Array(Box::new(self.resolve_type_name(inner, span)?)))
            }
            TypeName::Result(inner) => {
                Ok(Type::Result(Box::new(self.resolve_type_name(inner, span)?)))
            }
            TypeName::Custom(name) if self.structs.contains_key(name) => {
                Ok(Type::Struct(name.clone()))
            }
            TypeName::Custom(name) if self.enums.contains_key(name) => Ok(Type::Enum(name.clone())),
            TypeName::Custom(name) => {
                Err(KuError::runtime(format!("undefined type '{name}'"), span))
            }
        }
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
                let actual = self.check_expr(value)?;
                let expected = ty
                    .as_ref()
                    .map(|ty| self.resolve_type_name(ty, *span))
                    .transpose()?
                    .unwrap_or_else(|| actual.clone());
                if !type_matches(&expected, &actual) {
                    return Err(type_error(*span, &expected, &actual));
                }
                self.define(
                    name.clone(),
                    expected,
                    *mutable && !is_constant_name(name),
                    *span,
                )
            }
            Stmt::Assign { name, value, span } => {
                let actual = self.check_expr(value)?;
                if !self.contains(name) {
                    return self.define(name.clone(), actual, !is_constant_name(name), *span);
                }
                let binding = self.get(name, *span)?;
                if !binding.mutable {
                    return Err(KuError::runtime(
                        format!("cannot assign to immutable variable '{name}'"),
                        *span,
                    ));
                }
                if !type_matches(&binding.ty, &actual) {
                    return Err(type_error(*span, &binding.ty, &actual));
                }
                Ok(())
            }
            Stmt::AssignTarget {
                target,
                value,
                span,
            } => {
                let expected = self.check_assign_target(target, *span)?;
                let actual = self.check_expr(value)?;
                if !type_matches(&expected, &actual) {
                    return Err(type_error(*span, &expected, &actual));
                }
                Ok(())
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                self.expect_bool(condition, *span)?;
                self.check_block(then_branch)?;
                self.check_block(else_branch)
            }
            Stmt::While {
                condition,
                body,
                span,
            } => {
                self.expect_bool(condition, *span)?;
                self.check_block(body)
            }
            Stmt::For {
                name,
                iterable,
                body,
                span,
            } => {
                let iterable = self.check_expr(iterable)?;
                let Type::Array(element) = iterable else {
                    return Err(KuError::runtime(
                        format!(
                            "type error: for expects array but got {}",
                            type_name(&iterable)
                        ),
                        *span,
                    ));
                };
                self.push_scope();
                self.define(name.clone(), *element, true, *span)?;
                for stmt in body {
                    self.check_stmt(stmt)?;
                }
                self.pop_scope();
                Ok(())
            }
            Stmt::Function(function) => self.check_local_function(function),
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                span,
            } => {
                self.recoverable_depth += 1;
                let body_result = self.check_block(body);
                self.recoverable_depth -= 1;
                body_result?;
                if let Some(name) = catch_name {
                    self.push_scope();
                    self.define(name.clone(), Type::String, false, *span)?;
                    for stmt in catch_body {
                        self.check_stmt(stmt)?;
                    }
                    self.pop_scope();
                }
                self.check_block(finally_body)
            }
            Stmt::Fail { value, span } => {
                let actual = self.check_expr(value)?;
                if actual != Type::String {
                    return Err(type_error(*span, &Type::String, &actual));
                }
                match &self.current_return {
                    Type::Result(_) => Ok(()),
                    _ if self.recoverable_depth > 0 => Ok(()),
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
                let actual = match value {
                    Some(value) => self.check_expr(value)?,
                    None => Type::Void,
                };
                if !type_matches(&self.current_return, &actual) {
                    return Err(type_error(*span, &self.current_return, &actual));
                }
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
        }
        self.pop_scope();
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
                ExprKind::Variable(name) => self.get(name, expr.span).map(|v| v.ty),
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
                    if let Some(ty) = self.check_dotted_builtin_call(callee, args, expr.span)? {
                        return Ok(ty);
                    }
                    if let Some((enum_name, variant)) = enum_variant_path(callee) {
                        if self.enums.contains_key(&enum_name) {
                            return self
                                .check_enum_constructor(&enum_name, &variant, args, expr.span);
                        }
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
                            for (arg, expected) in args.iter().zip(function.params.iter()) {
                                let actual = self.check_expr(arg)?;
                                if !type_matches(expected, &actual) {
                                    return Err(type_error(arg.span, expected, &actual));
                                }
                            }
                            return Ok(function.returns);
                        }
                        if self.contains(name) {
                            let callee_type = self.get(name, callee.span)?.ty;
                            if let Type::FunctionValue {
                                params,
                                return_type,
                                body,
                            } = callee_type
                            {
                                return self.check_function_value_call(
                                    &params,
                                    return_type.as_deref(),
                                    &body,
                                    args,
                                    expr.span,
                                    Some(name),
                                );
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
                        let callee_type = self.check_expr(callee)?;
                        if let Type::FunctionValue {
                            params,
                            return_type,
                            body,
                        } = callee_type
                        {
                            self.check_function_value_call(
                                &params,
                                return_type.as_deref(),
                                &body,
                                args,
                                expr.span,
                                None,
                            )
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
                        let actual = self.check_expr(value)?;
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
                    if index_type != Type::Int {
                        return Err(type_error(index.span, &Type::Int, &index_type));
                    }
                    match target_type {
                        Type::Array(element) => Ok(*element),
                        other => Err(KuError::runtime(
                            format!("type error: cannot index {}", type_name(&other)),
                            target.span,
                        )),
                    }
                }
                ExprKind::Field { target, name } => {
                    if let ExprKind::Variable(module) = &target.kind {
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
                    match target_type {
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
                        let actual = self.check_expr(value)?;
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
                        object_fields.insert(field_name.clone(), self.check_expr(value)?);
                    }
                    Ok(Type::Object(object_fields))
                }
                ExprKind::Match { value, arms } => self.check_match_expr(value, arms, expr.span),
                ExprKind::TryUnwrap { expr: inner } => match self.check_expr(inner)? {
                    Type::Result(value) => {
                        if !matches!(self.current_return, Type::Result(_))
                            && self.recoverable_depth == 0
                        {
                            return Err(KuError::runtime(
                                "'?' requires a Result return type or an enclosing try block",
                                expr.span,
                            ));
                        }
                        Ok(*value)
                    }
                    other => Err(KuError::runtime(
                        format!("'?' expects Result but got {}", type_name(&other)),
                        expr.span,
                    )),
                },
                ExprKind::Function {
                    params,
                    return_type,
                    body,
                } => {
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
                        .map(|ty| self.resolve_type_name(ty, expr.span).map(Box::new))
                        .transpose()?;
                    let arg_types = params
                        .iter()
                        .map(|param| param.ty.clone().unwrap_or(Type::Unknown))
                        .collect::<Vec<_>>();
                    self.check_function_value_body(
                        &params,
                        return_type.as_deref(),
                        body,
                        &arg_types,
                        expr.span,
                    )?;
                    Ok(Type::FunctionValue {
                        params,
                        return_type,
                        body: body.clone(),
                    })
                }
            }
        })();
        self.check_depth = self.check_depth.saturating_sub(1);
        result
    }

    fn check_binary(&self, op: BinaryOp, left: &Type, right: &Type, span: Span) -> KuResult<Type> {
        match op {
            _ if left == &Type::Unknown || right == &Type::Unknown => Ok(Type::Unknown),
            BinaryOp::Add if self.template_mode && can_template_concat(left, right) => {
                Ok(Type::String)
            }
            BinaryOp::Add if left == &Type::String && right == &Type::String => Ok(Type::String),
            BinaryOp::Add
            | BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Remainder => numeric_result(op, left, right, span),
            BinaryOp::Equal | BinaryOp::NotEqual if type_matches(left, right) => Ok(Type::Bool),
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
        match name {
            "len" => {
                expect_arg_count(name, args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String || matches!(actual, Type::Array(_)) {
                    Ok(Some(Type::Int))
                } else {
                    Err(type_error(args[0].span, &Type::String, &actual))
                }
            }
            "str" => {
                expect_arg_count(name, args.len(), 1, span)?;
                self.check_expr(&args[0])?;
                Ok(Some(Type::String))
            }
            "ok" => {
                expect_arg_count(name, args.len(), 1, span)?;
                Ok(Some(Type::Result(Box::new(self.check_expr(&args[0])?))))
            }
            "err" => {
                expect_arg_count(name, args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String {
                    Ok(Some(Type::Result(Box::new(Type::Unknown))))
                } else {
                    Err(type_error(args[0].span, &Type::String, &actual))
                }
            }
            _ => Ok(None),
        }
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
                if index_type != Type::Int {
                    return Err(type_error(index.span, &Type::Int, &index_type));
                }
                match target_type {
                    Type::Array(element) => Ok(*element),
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
                other => Err(KuError::runtime(
                    format!("type error: {} has no fields", type_name(&other)),
                    target.span,
                )),
            },
        }
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
            let actual = self.check_expr(arg)?;
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
        let mut result_type = Type::Unknown;
        let mut saw_wildcard = false;
        for arm in arms {
            if saw_wildcard {
                return Err(KuError::runtime(
                    "match arm after '_' is unreachable",
                    arm.span,
                ));
            }
            self.push_scope();
            match &arm.pattern {
                MatchPattern::Wildcard => saw_wildcard = true,
                MatchPattern::Literal(literal) => {
                    let literal_type = type_of_literal(literal);
                    if !type_matches(&value_type, &literal_type) {
                        self.pop_scope();
                        return Err(type_error(arm.span, &value_type, &literal_type));
                    }
                }
                MatchPattern::EnumVariant {
                    enum_name,
                    variant,
                    bindings,
                } => {
                    if !type_matches(&value_type, &Type::Enum(enum_name.clone())) {
                        self.pop_scope();
                        return Err(type_error(
                            arm.span,
                            &value_type,
                            &Type::Enum(enum_name.clone()),
                        ));
                    }
                    let Some(enum_type) = self.enums.get(enum_name) else {
                        self.pop_scope();
                        return Err(KuError::runtime(
                            format!("undefined enum '{enum_name}'"),
                            arm.span,
                        ));
                    };
                    let Some(payload) = enum_type.variants.get(variant).cloned() else {
                        self.pop_scope();
                        return Err(KuError::runtime(
                            format!("enum '{enum_name}' has no variant '{variant}'"),
                            arm.span,
                        ));
                    };
                    if payload.len() != bindings.len() {
                        self.pop_scope();
                        return Err(KuError::runtime(
                            format!(
                                "match pattern '{enum_name}.{variant}' expects {} bindings but got {}",
                                payload.len(),
                                bindings.len()
                            ),
                            arm.span,
                        ));
                    }
                    for (binding, ty) in bindings.iter().zip(payload) {
                        self.define(binding.clone(), ty, false, arm.span)?;
                    }
                }
            }
            if let Some(guard) = &arm.guard {
                let guard_type = self.check_expr(guard)?;
                if guard_type != Type::Bool {
                    self.pop_scope();
                    return Err(type_error(guard.span, &Type::Bool, &guard_type));
                }
            }
            let actual = self.check_expr(&arm.value);
            self.pop_scope();
            let actual = actual?;
            if result_type == Type::Unknown {
                result_type = actual;
            } else if !type_matches(&result_type, &actual) {
                return Err(type_error(arm.value.span, &result_type, &actual));
            }
        }
        Ok(result_type)
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
        match (module.as_str(), function.as_str()) {
            ("fs", "read") => {
                expect_arg_count("fs.read", args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String {
                    Ok(Some(Type::String))
                } else {
                    Err(type_error(args[0].span, &Type::String, &actual))
                }
            }
            ("fs", "try_read") => {
                expect_arg_count("fs.try_read", args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String {
                    Ok(Some(Type::Result(Box::new(Type::String))))
                } else {
                    Err(type_error(args[0].span, &Type::String, &actual))
                }
            }
            ("lexer", "scan") => {
                expect_arg_count("lexer.scan", args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String {
                    Ok(Some(Type::Array(Box::new(Type::String))))
                } else {
                    Err(type_error(args[0].span, &Type::String, &actual))
                }
            }
            ("parser", "parse") => {
                expect_arg_count("parser.parse", args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String || actual == Type::Array(Box::new(Type::String)) {
                    Ok(Some(Type::String))
                } else {
                    Err(KuError::runtime(
                        format!(
                            "type error: expected str or [str] but got {}",
                            type_name(&actual)
                        ),
                        args[0].span,
                    ))
                }
            }
            ("string", "len" | "trim" | "lower" | "upper") => {
                expect_arg_count(&format!("{module}.{function}"), args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual != Type::String {
                    return Err(type_error(args[0].span, &Type::String, &actual));
                }
                if function == "len" {
                    Ok(Some(Type::Int))
                } else {
                    Ok(Some(Type::String))
                }
            }
            ("string", "contains" | "starts_with" | "ends_with") => {
                expect_arg_count(&format!("{module}.{function}"), args.len(), 2, span)?;
                self.expect_string_args(args)?;
                Ok(Some(Type::Bool))
            }
            ("string", "replace") => {
                expect_arg_count("string.replace", args.len(), 3, span)?;
                self.expect_string_args(args)?;
                Ok(Some(Type::String))
            }
            ("array", "len") => {
                expect_arg_count("array.len", args.len(), 1, span)?;
                match self.check_expr(&args[0])? {
                    Type::Array(_) => Ok(Some(Type::Int)),
                    actual => Err(type_error(
                        args[0].span,
                        &Type::Array(Box::new(Type::Unknown)),
                        &actual,
                    )),
                }
            }
            ("array", "is_empty") => {
                expect_arg_count("array.is_empty", args.len(), 1, span)?;
                match self.check_expr(&args[0])? {
                    Type::Array(_) => Ok(Some(Type::Bool)),
                    actual => Err(type_error(
                        args[0].span,
                        &Type::Array(Box::new(Type::Unknown)),
                        &actual,
                    )),
                }
            }
            ("array", "push") => {
                expect_arg_count("array.push", args.len(), 2, span)?;
                match self.check_expr(&args[0])? {
                    Type::Array(element) => {
                        let value = self.check_expr(&args[1])?;
                        if !type_matches(&element, &value) {
                            return Err(type_error(args[1].span, &element, &value));
                        }
                        Ok(Some(Type::Array(element)))
                    }
                    actual => Err(type_error(
                        args[0].span,
                        &Type::Array(Box::new(Type::Unknown)),
                        &actual,
                    )),
                }
            }
            ("array", "concat") => {
                expect_arg_count("array.concat", args.len(), 2, span)?;
                let left = self.check_expr(&args[0])?;
                let right = self.check_expr(&args[1])?;
                match (&left, &right) {
                    (Type::Array(left), Type::Array(right)) if type_matches(left, right) => {
                        Ok(Some(Type::Array(left.clone())))
                    }
                    (Type::Array(_), Type::Array(_)) => {
                        Err(type_error(args[1].span, &left, &right))
                    }
                    _ => Err(type_error(
                        args[0].span,
                        &Type::Array(Box::new(Type::Unknown)),
                        &left,
                    )),
                }
            }
            ("array", "first" | "last") => {
                expect_arg_count(&format!("{module}.{function}"), args.len(), 1, span)?;
                match self.check_expr(&args[0])? {
                    Type::Array(element) => Ok(Some(*element)),
                    actual => Err(type_error(
                        args[0].span,
                        &Type::Array(Box::new(Type::Unknown)),
                        &actual,
                    )),
                }
            }
            ("json", "parse") => {
                expect_arg_count("json.parse", args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String {
                    Ok(Some(Type::Unknown))
                } else {
                    Err(type_error(args[0].span, &Type::String, &actual))
                }
            }
            ("json", "try_parse") => {
                expect_arg_count("json.try_parse", args.len(), 1, span)?;
                let actual = self.check_expr(&args[0])?;
                if actual == Type::String {
                    Ok(Some(Type::Result(Box::new(Type::Unknown))))
                } else {
                    Err(type_error(args[0].span, &Type::String, &actual))
                }
            }
            ("json", "stringify") => {
                expect_arg_count("json.stringify", args.len(), 1, span)?;
                self.check_expr(&args[0])?;
                Ok(Some(Type::String))
            }
            ("time", "now" | "unix" | "millis") => {
                expect_arg_count(&format!("{module}.{function}"), args.len(), 0, span)?;
                Ok(Some(Type::Int))
            }
            _ => Ok(None),
        }
    }

    fn expect_string_args(&mut self, args: &[Expr]) -> KuResult<()> {
        for arg in args {
            let actual = self.check_expr(arg)?;
            if actual != Type::String {
                return Err(type_error(arg.span, &Type::String, &actual));
            }
        }
        Ok(())
    }

    fn check_function_value_call(
        &mut self,
        params: &[FunctionValueParam],
        return_type: Option<&Type>,
        body: &[Stmt],
        args: &[Expr],
        span: Span,
        name: Option<&str>,
    ) -> KuResult<Type> {
        if params.len() != args.len() {
            let subject = name
                .map(|name| format!("function value '{name}'"))
                .unwrap_or_else(|| "function value".to_string());
            return Err(KuError::runtime(
                format!(
                    "{subject} expects {} arguments but got {}",
                    params.len(),
                    args.len()
                ),
                span,
            ));
        }
        let actual_arg_types = args
            .iter()
            .map(|arg| self.check_expr(arg))
            .collect::<KuResult<Vec<_>>>()?;
        let mut arg_types = Vec::new();
        for ((param, actual), arg) in params.iter().zip(actual_arg_types.iter()).zip(args.iter()) {
            if let Some(expected) = &param.ty {
                if !type_matches(expected, actual) {
                    return Err(type_error(arg.span, expected, actual));
                }
                arg_types.push(expected.clone());
            } else {
                arg_types.push(actual.clone());
            }
        }
        self.check_function_value_body(params, return_type, body, &arg_types, span)
    }

    fn check_function_value_body(
        &mut self,
        params: &[FunctionValueParam],
        return_type: Option<&Type>,
        body: &[Stmt],
        arg_types: &[Type],
        span: Span,
    ) -> KuResult<Type> {
        let saved_return = self.current_return.clone();
        self.current_return = return_type.cloned().unwrap_or(Type::Unknown);
        self.push_scope();

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

        self.pop_scope();
        self.current_return = saved_return;
        result
    }

    fn check_local_function(&mut self, function: &FnDecl) -> KuResult<()> {
        reject_duplicate_params(function)?;
        let params = function
            .params
            .iter()
            .map(|param| {
                Ok(FunctionValueParam {
                    name: param.name.clone(),
                    ty: Some(self.resolve_type_name(&param.ty, param.span)?),
                })
            })
            .collect::<KuResult<Vec<_>>>()?;
        let return_type = function
            .return_type
            .as_ref()
            .map(|ty| self.resolve_type_name(ty, function.span))
            .transpose()?;
        self.define(
            function.name.clone(),
            Type::FunctionValue {
                params: params.clone(),
                return_type: return_type.clone().map(Box::new),
                body: function.body.clone(),
            },
            false,
            function.span,
        )?;
        let arg_types = params
            .iter()
            .map(|param| param.ty.clone().unwrap_or(Type::Unknown))
            .collect::<Vec<_>>();
        self.check_function_value_body(
            &params,
            return_type.as_ref(),
            &function.body,
            &arg_types,
            function.span,
        )?;
        Ok(())
    }

    fn check_stmt_and_infer_return(&mut self, stmt: &Stmt) -> KuResult<Option<Type>> {
        match stmt {
            Stmt::Return { value, span } => {
                let actual = match value {
                    Some(value) => self.check_expr(value)?,
                    None => Type::Null,
                };
                if !type_matches(&self.current_return, &actual) {
                    return Err(type_error(*span, &self.current_return, &actual));
                }
                Ok(Some(actual))
            }
            Stmt::Fail { value, span } => {
                let actual = self.check_expr(value)?;
                if actual != Type::String {
                    return Err(type_error(*span, &Type::String, &actual));
                }
                if !matches!(self.current_return, Type::Result(_)) {
                    if self.recoverable_depth > 0 {
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
                Ok(Some(self.current_return.clone()))
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                self.expect_bool(condition, *span)?;
                let then_return = self.check_block_and_infer_return(then_branch)?;
                let else_return = self.check_block_and_infer_return(else_branch)?;
                match (then_return, else_return) {
                    (Some(left), Some(right)) => {
                        Ok(Some(merge_return_types(&left, &right, *span)?))
                    }
                    (Some(left), None) | (None, Some(left)) => Ok(Some(left)),
                    (None, None) => Ok(None),
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

    fn expect_bool(&mut self, expr: &Expr, span: Span) -> KuResult<()> {
        let ty = self.check_expr(expr)?;
        if ty == Type::Bool {
            Ok(())
        } else {
            Err(type_error(span, &Type::Bool, &ty))
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, ty: Type, mutable: bool, span: Span) -> KuResult<()> {
        let scope = self.scopes.last_mut().expect("checker always has a scope");
        if scope.contains_key(&name) {
            return Err(KuError::runtime(
                format!("variable '{name}' is already defined in this scope"),
                span,
            ));
        }
        scope.insert(name, VarType { ty, mutable });
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
                return Ok(var.clone());
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
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
    matches!(ty, Type::Int | Type::Float)
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
    if left == &Type::Float || right == &Type::Float {
        Ok(Type::Float)
    } else {
        Ok(Type::Int)
    }
}

fn type_matches(expected: &Type, actual: &Type) -> bool {
    match (expected, actual) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Array(left), Type::Array(right)) => type_matches(left, right),
        (Type::Result(left), Type::Result(right)) => type_matches(left, right),
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

fn type_of_literal(literal: &Literal) -> Type {
    match literal {
        Literal::Int(_) => Type::Int,
        Literal::Float(_) => Type::Float,
        Literal::Bool(_) => Type::Bool,
        Literal::String(_) | Literal::TemplateString(_) => Type::String,
        Literal::Null => Type::Null,
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

fn type_name(ty: &Type) -> String {
    match ty {
        Type::Int => "int".to_string(),
        Type::Float => "float".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "str".to_string(),
        Type::Null => "null".to_string(),
        Type::Array(inner) => format!("[{}]", type_name(inner)),
        Type::Result(inner) => format!("{}!", type_name(inner)),
        Type::Object(_) => "object".to_string(),
        Type::Struct(name) => name.clone(),
        Type::Enum(name) => name.clone(),
        Type::Void => "void".to_string(),
        Type::FunctionValue { .. } => "function".to_string(),
        Type::Unknown => "unknown".to_string(),
    }
}

fn can_template_concat(left: &Type, right: &Type) -> bool {
    matches!(
        (left, right),
        (Type::String, Type::Int | Type::Float)
            | (Type::Int | Type::Float, Type::String)
            | (Type::String, Type::String)
    )
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

fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::VarDecl { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::AssignTarget { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
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

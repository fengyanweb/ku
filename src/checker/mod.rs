use std::collections::{HashMap, HashSet};

use crate::{
    ast::*,
    error::{KuError, KuResult},
    span::Span,
    stdlib::metadata::{self, ArgRule, Signature, TypePattern},
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
    Union(Vec<Type>),
    Object(HashMap<String, Type>),
    Struct(String),
    Enum(String),
    Generic(String),
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
    type_params: Vec<String>,
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
    loop_depth: usize,
    template_mode: bool,
    std_modules: HashSet<String>,
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
            template_mode: false,
            std_modules: HashSet::new(),
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
                    if let Some(name) = decl.name.strip_prefix("std:") {
                        self.std_modules.insert(name.to_string());
                        continue;
                    }
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
                let actuals = values
                    .iter()
                    .map(|value| self.check_expr(value))
                    .collect::<KuResult<Vec<_>>>()?;
                for (name, actual) in names.iter().zip(actuals) {
                    let Some(name) = name else {
                        continue;
                    };
                    if !self.contains(name) {
                        self.define(name.clone(), actual, !is_constant_name(name), *span)?;
                        continue;
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
                }
                Ok(())
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                self.expect_condition(condition, *span)?;
                self.check_block(then_branch)?;
                self.check_block(else_branch)
            }
            Stmt::While {
                condition,
                body,
                span,
            } => {
                self.expect_condition(condition, *span)?;
                self.loop_depth += 1;
                let result = self.check_block(body);
                self.loop_depth -= 1;
                result
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
                self.loop_depth += 1;
                let result = (|| -> KuResult<()> {
                    self.define(name.clone(), *element, true, *span)?;
                    for stmt in body {
                        self.check_stmt(stmt)?;
                    }
                    Ok(())
                })();
                self.loop_depth -= 1;
                self.pop_scope();
                result
            }
            Stmt::Break { span } => {
                if self.loop_depth == 0 {
                    Err(KuError::runtime("break outside loop", *span))
                } else {
                    Ok(())
                }
            }
            Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    Err(KuError::runtime("continue outside loop", *span))
                } else {
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
                self.recoverable_depth += 1;
                let body_result = self.check_block(body);
                self.recoverable_depth -= 1;
                body_result?;
                if let Some(name) = catch_name {
                    self.push_scope();
                    self.define(name.clone(), error_type(), false, *span)?;
                    for stmt in catch_body {
                        self.check_stmt(stmt)?;
                    }
                    self.pop_scope();
                }
                self.check_block(finally_body)
            }
            Stmt::Fail { value, span } => {
                let actual = self.check_expr(value)?;
                if actual != Type::String && !matches!(actual, Type::Object(_)) {
                    return Err(type_error(*span, &error_type(), &actual));
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
                            let mut generic_bindings = HashMap::new();
                            for (arg, expected) in args.iter().zip(function.params.iter()) {
                                let actual = self.check_expr(arg)?;
                                if !bind_generic_type(expected, &actual, &mut generic_bindings)
                                    || !type_matches(expected, &actual)
                                {
                                    return Err(type_error(arg.span, expected, &actual));
                                }
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
                            return Ok(substitute_generics(&function.returns, &generic_bindings));
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
                        if let Some(ty) =
                            self.check_http_service_method_call(callee, args, expr.span)?
                        {
                            return Ok(ty);
                        }
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
                            && matches!(name.as_str(), "service" | "server")
                        {
                            return Ok(http_service_type());
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
            BinaryOp::Add if self.template_mode && can_template_concat(left, right) => {
                Ok(Type::String)
            }
            BinaryOp::Add if left == &Type::String && right == &Type::String => Ok(Type::String),
            BinaryOp::Equal | BinaryOp::NotEqual if type_matches(left, right) => Ok(Type::Bool),
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
                    Type::Object(_) => {
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
        if !matches!(name.as_str(), "get" | "post" | "put" | "del" | "listen") {
            return Ok(None);
        }
        let target_type = self.check_expr(target)?;
        if !is_http_service_type(&target_type) {
            return Ok(None);
        }
        if name == "listen" {
            if args.is_empty() || args.len() > 2 {
                return Err(KuError::runtime(
                    format!(
                        "http service listen expects 1 or 2 arguments but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let address = self.check_expr(&args[0])?;
            if !type_matches(&Type::String, &address) {
                return Err(type_error(args[0].span, &Type::String, &address));
            }
            if let Some(config) = args.get(1) {
                let config_type = self.check_expr(config)?;
                if !matches!(config_type, Type::Object(_) | Type::Unknown) {
                    return Err(type_error(
                        config.span,
                        &Type::Object(HashMap::new()),
                        &config_type,
                    ));
                }
            }
            return Ok(Some(Type::Result(Box::new(Type::Null))));
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
        let handler_type = self.check_expr(&args[1])?;
        if !matches!(handler_type, Type::FunctionValue { .. } | Type::Unknown) {
            return Err(KuError::runtime(
                format!("http service {name} handler must be a function"),
                args[1].span,
            ));
        }
        Ok(Some(target_type))
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
        let mut saw_unguarded_catch_all = false;
        let mut covered_full_variants = HashSet::new();
        let mut covered_patterns = HashSet::new();
        for arm in arms {
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
            let actual = self.check_expr(&arm.value);
            self.pop_scope();
            let actual = actual?;
            if result_type == Type::Unknown {
                result_type = actual;
            } else if !type_matches(&result_type, &actual) {
                return Err(type_error(arm.value.span, &result_type, &actual));
            }
        }
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
        Ok(result_type)
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
        let module = match target_type {
            Type::String => "string",
            Type::Array(_) if name != "map" => "array",
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
        expect_arg_count(&signature.name, args.len(), signature.args.len(), span)?;
        let actuals = args
            .iter()
            .map(|arg| self.check_expr(arg))
            .collect::<KuResult<Vec<_>>>()?;
        for (index, rule) in signature.args.iter().enumerate() {
            self.check_stdlib_arg(rule, index, args, &actuals)?;
        }
        self.stdlib_pattern_to_type(&signature.returns, &actuals, span)
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
                if self.type_matches_pattern(&actuals[index], pattern) {
                    Ok(())
                } else {
                    Err(type_error(
                        args[index].span,
                        &self.pattern_expected_type(pattern),
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

    fn type_matches_pattern(&self, actual: &Type, pattern: &TypePattern) -> bool {
        if let Type::Union(types) = actual {
            return types
                .iter()
                .all(|actual| self.type_matches_pattern(actual, pattern));
        }
        match pattern {
            TypePattern::Int => actual == &Type::Int,
            TypePattern::Bool => actual == &Type::Bool,
            TypePattern::String => actual == &Type::String,
            TypePattern::Null => actual == &Type::Null,
            TypePattern::Unknown | TypePattern::Any => true,
            TypePattern::ArrayAny => matches!(actual, Type::Array(_)),
            TypePattern::ObjectAny => matches!(actual, Type::Object(_)),
            TypePattern::ObjectFields(fields) => match actual {
                Type::Object(actual_fields) => fields.iter().all(|(name, pattern)| {
                    actual_fields
                        .get(name)
                        .is_some_and(|actual| self.type_matches_pattern(actual, pattern))
                }),
                _ => false,
            },
            TypePattern::StringOrStringArray => {
                actual == &Type::String || actual == &Type::Array(Box::new(Type::String))
            }
            TypePattern::ArrayOf(inner) => match actual {
                Type::Array(element) => self.type_matches_pattern(element, inner),
                _ => false,
            },
            TypePattern::ArrayElementOfArg(_)
            | TypePattern::ResultOf(_)
            | TypePattern::SameAsArg(_) => true,
        }
    }

    fn pattern_expected_type(&self, pattern: &TypePattern) -> Type {
        match pattern {
            TypePattern::Int => Type::Int,
            TypePattern::Bool => Type::Bool,
            TypePattern::String => Type::String,
            TypePattern::Null => Type::Null,
            TypePattern::ArrayAny => Type::Array(Box::new(Type::Unknown)),
            TypePattern::ObjectAny => Type::Object(HashMap::new()),
            TypePattern::ObjectFields(fields) => Type::Object(
                fields
                    .iter()
                    .map(|(name, pattern)| (name.clone(), self.pattern_expected_type(pattern)))
                    .collect(),
            ),
            TypePattern::ArrayOf(inner) => Type::Array(Box::new(self.pattern_expected_type(inner))),
            TypePattern::StringOrStringArray => Type::String,
            TypePattern::Unknown
            | TypePattern::Any
            | TypePattern::ArrayElementOfArg(_)
            | TypePattern::ResultOf(_)
            | TypePattern::SameAsArg(_) => Type::Unknown,
        }
    }

    fn stdlib_pattern_to_type(
        &self,
        pattern: &TypePattern,
        actuals: &[Type],
        span: Span,
    ) -> KuResult<Type> {
        match pattern {
            TypePattern::Int => Ok(Type::Int),
            TypePattern::Bool => Ok(Type::Bool),
            TypePattern::String => Ok(Type::String),
            TypePattern::Null => Ok(Type::Null),
            TypePattern::Unknown | TypePattern::Any => Ok(Type::Unknown),
            TypePattern::ArrayAny => Ok(Type::Array(Box::new(Type::Unknown))),
            TypePattern::ObjectAny => Ok(Type::Object(HashMap::new())),
            TypePattern::ObjectFields(fields) => Ok(Type::Object(
                fields
                    .iter()
                    .map(|(name, pattern)| {
                        Ok((
                            name.clone(),
                            self.stdlib_pattern_to_type(pattern, actuals, span)?,
                        ))
                    })
                    .collect::<KuResult<HashMap<_, _>>>()?,
            )),
            TypePattern::ArrayOf(inner) => Ok(Type::Array(Box::new(
                self.stdlib_pattern_to_type(inner, actuals, span)?,
            ))),
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
                self.stdlib_pattern_to_type(inner, actuals, span)?,
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
        let mapper_type = self.check_expr(&args[0])?;
        let Type::FunctionValue {
            params,
            return_type,
            body,
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
            &params,
            return_type.as_deref(),
            &body,
            &[*element],
            span,
        )?;
        Ok(Some(Type::Array(Box::new(mapped))))
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
        self.check_function_value_call_with_types(params, return_type, body, &arg_types, span)
    }

    fn check_function_value_call_with_types(
        &mut self,
        params: &[FunctionValueParam],
        return_type: Option<&Type>,
        body: &[Stmt],
        arg_types: &[Type],
        span: Span,
    ) -> KuResult<Type> {
        if params.len() != arg_types.len() {
            return Err(KuError::runtime(
                format!(
                    "function value expects {} arguments but got {}",
                    params.len(),
                    arg_types.len()
                ),
                span,
            ));
        }
        for (param, actual) in params.iter().zip(arg_types.iter()) {
            if let Some(expected) = &param.ty {
                if !type_matches(expected, actual) {
                    return Err(type_error(span, expected, actual));
                }
            }
        }
        if let Some(return_type) = return_type {
            return Ok(return_type.clone());
        }
        self.check_function_value_body(params, return_type, body, arg_types, span)
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
                    None if self.current_return == Type::Void => Type::Void,
                    None => Type::Null,
                };
                if !type_matches(&self.current_return, &actual) {
                    return Err(type_error(*span, &self.current_return, &actual));
                }
                Ok(Some(actual))
            }
            Stmt::Fail { value, span } => {
                let actual = self.check_expr(value)?;
                if actual != Type::String && !matches!(actual, Type::Object(_)) {
                    return Err(type_error(*span, &error_type(), &actual));
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
                self.expect_condition(condition, *span)?;
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
        (Type::Union(options), _) => options.iter().any(|option| type_matches(option, actual)),
        (_, Type::Union(options)) => options.iter().all(|option| type_matches(expected, option)),
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
        Type::Union(types) => types.iter().map(type_name).collect::<Vec<_>>().join(" | "),
        Type::Object(_) => "object".to_string(),
        Type::Struct(name) => name.clone(),
        Type::Enum(name) => name.clone(),
        Type::Generic(name) => name.clone(),
        Type::Void => "void".to_string(),
        Type::FunctionValue { .. } => "function".to_string(),
        Type::Unknown => "unknown".to_string(),
    }
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

fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::VarDecl { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::AssignTarget { span, .. }
        | Stmt::DestructureAssign { span, .. }
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

fn http_service_type() -> Type {
    Type::Object(HashMap::from([
        ("kind".to_string(), Type::String),
        ("read_timeout_ms".to_string(), Type::Int),
        ("write_timeout_ms".to_string(), Type::Int),
        ("max_body_bytes".to_string(), Type::Int),
        ("max_header_bytes".to_string(), Type::Int),
        ("max_connections".to_string(), Type::Int),
        ("max_concurrency".to_string(), Type::Int),
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
        ("handler".to_string(), Type::Unknown),
    ]))
}

fn is_http_service_type(ty: &Type) -> bool {
    let Type::Object(fields) = ty else {
        return false;
    };
    matches!(fields.get("kind"), Some(Type::String))
        && fields.contains_key("routes")
        && fields.contains_key("max_concurrency")
        && fields.contains_key("max_body_bytes")
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

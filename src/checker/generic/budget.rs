//! An upper-bound payload/work ledger, not an allocator/RSS measurement.
//! Visit every cloneable AST/type payload before retaining a new instance. The
//! two stored request copies are reserved together; no request is cloned first.
use super::*;
use std::mem::size_of;

enum Node<'a> {
    Statement(&'a Stmt),
    Expression(&'a Expr),
    Target(&'a AssignTarget),
    Function(&'a FnDecl),
    Parameter(&'a Param),
    FunctionParameter(&'a FunctionParam),
    Pattern(&'a MatchPattern),
    AstType(&'a TypeName),
    CheckedType(&'a Type),
    CheckedParameter(&'a FunctionValueParam),
    Text(&'a str),
}

struct Pending<'a> {
    nodes: Vec<Node<'a>>,
    limit: usize,
    overflow: bool,
}

impl<'a> Pending<'a> {
    fn new(limit: usize) -> Self {
        Self {
            nodes: Vec::new(),
            limit,
            overflow: false,
        }
    }
    fn push(&mut self, node: Node<'a>) {
        if self.nodes.len() >= self.limit {
            self.overflow = true;
        } else {
            self.nodes.push(node);
        }
    }
    fn extend(&mut self, nodes: impl IntoIterator<Item = Node<'a>>) {
        for node in nodes {
            self.push(node);
            if self.overflow {
                break;
            }
        }
    }
    fn pop(&mut self) -> Option<Node<'a>> {
        self.nodes.pop()
    }
    fn len(&self) -> usize {
        self.nodes.len()
    }
}

pub(super) fn instance_cost(
    function: &FunctionType,
    bindings: &HashMap<String, Type>,
    arguments: &[Type],
    span: Span,
) -> KuResult<(usize, usize)> {
    if function.body.len() > MAX_INSTANCE_BODY_NODES / 2
        || arguments.len() > MAX_INSTANCE_BODY_NODES / 2
        || bindings.len() > MAX_INSTANCE_BODY_NODES / 2
    {
        return Err(limit(span, "initial AST/type work"));
    }
    let mut pending = Pending::new(MAX_INSTANCE_BODY_NODES / 2);
    pending.extend(function.body.iter().map(Node::Statement));
    pending.extend(function.params.iter().map(Node::CheckedType));
    pending.extend(function.value_params.iter().map(Node::CheckedParameter));
    pending.extend(function.type_params.iter().map(|name| Node::Text(name)));
    pending.push(Node::CheckedType(&function.returns));
    pending.extend(function.return_type.iter().map(Node::CheckedType));
    for (name, ty) in bindings {
        pending.push(Node::Text(name));
        pending.push(Node::CheckedType(ty));
    }
    pending.extend(arguments.iter().map(Node::CheckedType));
    let (nodes, bytes, _) = walk(pending, span, size_of::<FunctionType>(), false)?;
    Ok((nodes * 2, bytes * 2))
}

// Share the complete AST visitor with native declaration discovery. In
// particular, local declarations inside anonymous callbacks are not skipped.
pub(super) fn local_generic_span(body: &[Stmt], span: Span) -> KuResult<Option<Span>> {
    if body.len() > MAX_INSTANCE_BODY_NODES {
        return Err(limit(span, "declaration scan pending work"));
    }
    let mut pending = Pending::new(4 * MAX_INSTANCE_BODY_NODES);
    pending.extend(body.iter().map(Node::Statement));
    Ok(walk(pending, span, 0, true)?.2)
}

fn walk(
    mut pending: Pending<'_>,
    span: Span,
    mut bytes: usize,
    scanning: bool,
) -> KuResult<(usize, usize, Option<Span>)> {
    let mut nodes = 0usize;
    let node_limit = if scanning {
        4 * MAX_INSTANCE_BODY_NODES
    } else {
        MAX_INSTANCE_BODY_NODES / 2
    };
    while let Some(node) = pending.pop() {
        nodes = nodes
            .checked_add(1)
            .ok_or_else(|| limit(span, "AST/type work"))?;
        let header = match node {
            Node::Statement(_) => size_of::<Stmt>(),
            Node::Expression(_) => size_of::<Expr>(),
            Node::Target(_) => size_of::<AssignTarget>(),
            Node::Function(_) => size_of::<FnDecl>(),
            Node::Parameter(_) => size_of::<Param>(),
            Node::FunctionParameter(_) => size_of::<FunctionParam>(),
            Node::Pattern(_) => size_of::<MatchPattern>(),
            Node::AstType(_) => size_of::<TypeName>(),
            Node::CheckedType(_) => size_of::<Type>(),
            Node::CheckedParameter(_) => size_of::<FunctionValueParam>(),
            Node::Text(text) => size_of::<String>()
                .checked_add(text.len())
                .ok_or_else(|| limit(span, "string bytes"))?,
        };
        if !scanning {
            bytes = bytes
                .checked_add(header)
                .ok_or_else(|| limit(span, "AST/type payload bytes"))?;
        }
        if pending.overflow
            || nodes > node_limit
            || (!scanning && bytes > MAX_INSTANCE_BODY_BYTES / 2)
            || pending.len() > node_limit
        {
            return Err(limit(span, "AST/type payload bytes or nodes"));
        }
        match node {
            Node::Text(_) => {}
            Node::Function(function) => {
                if scanning && !function.type_params.is_empty() {
                    return Ok((nodes, bytes, Some(function.span)));
                }
                pending.push(Node::Text(&function.name));
                pending.extend(function.type_params.iter().map(|name| Node::Text(name)));
                pending.extend(function.params.iter().map(Node::Parameter));
                pending.extend(function.return_type.iter().map(Node::AstType));
                pending.extend(function.body.iter().map(Node::Statement));
            }
            Node::Parameter(param) => {
                pending.push(Node::Text(&param.name));
                pending.extend(param.ty.iter().map(Node::AstType));
            }
            Node::FunctionParameter(param) => {
                pending.push(Node::Text(&param.name));
                pending.extend(param.ty.iter().map(Node::AstType));
            }
            Node::CheckedParameter(param) => {
                pending.push(Node::Text(&param.name));
                pending.extend(param.ty.iter().map(Node::CheckedType));
            }
            Node::AstType(ty) => match ty {
                TypeName::Custom(name) => pending.push(Node::Text(name)),
                TypeName::Array(inner) | TypeName::Result(inner) => {
                    pending.push(Node::AstType(inner))
                }
                TypeName::Function {
                    params,
                    param_modes,
                    return_type,
                    ..
                } => {
                    bytes = bytes
                        .checked_add(param_modes.len() * size_of::<ParamMode>())
                        .ok_or_else(|| limit(span, "parameter modes"))?;
                    pending.extend(params.iter().map(Node::AstType));
                    pending.push(Node::AstType(return_type));
                }
                TypeName::Union(types) => pending.extend(types.iter().map(Node::AstType)),
                TypeName::Int
                | TypeName::Float
                | TypeName::Bool
                | TypeName::String
                | TypeName::Null => {}
            },
            Node::CheckedType(ty) => match ty {
                Type::Generic(name)
                | Type::Struct(name)
                | Type::Enum(name)
                | Type::Native(name) => pending.push(Node::Text(name)),
                Type::Array(inner) | Type::Result(inner) | Type::Task(inner) => {
                    pending.push(Node::CheckedType(inner))
                }
                Type::Union(types) => pending.extend(types.iter().map(Node::CheckedType)),
                Type::Object(fields) => {
                    for (name, ty) in fields {
                        pending.push(Node::Text(name));
                        pending.push(Node::CheckedType(ty));
                    }
                }
                Type::FunctionValue {
                    params,
                    return_type,
                    body,
                    ..
                } => {
                    pending.extend(params.iter().map(Node::CheckedParameter));
                    pending.extend(return_type.iter().map(|ty| Node::CheckedType(ty)));
                    pending.extend(body.iter().map(Node::Statement));
                }
                Type::Int
                | Type::Float
                | Type::Bool
                | Type::String
                | Type::Null
                | Type::StringMap
                | Type::DynamicObject
                | Type::KuValue
                | Type::Unknown
                | Type::Void => {}
            },
            Node::Target(target) => match target {
                AssignTarget::Variable(name) => pending.push(Node::Text(name)),
                AssignTarget::Field { target, name } => {
                    pending.push(Node::Expression(target));
                    pending.push(Node::Text(name));
                }
                AssignTarget::Index { target, index } => {
                    pending.push(Node::Expression(target));
                    pending.push(Node::Expression(index));
                }
            },
            Node::Pattern(pattern) => match pattern {
                MatchPattern::Binding(name) => pending.push(Node::Text(name)),
                MatchPattern::EnumVariant {
                    enum_name,
                    variant,
                    fields,
                } => {
                    pending.push(Node::Text(enum_name));
                    pending.push(Node::Text(variant));
                    pending.extend(fields.iter().map(Node::Pattern));
                }
                MatchPattern::Literal(Literal::String(text) | Literal::TemplateString(text)) => {
                    pending.push(Node::Text(text))
                }
                MatchPattern::Literal(_) | MatchPattern::Wildcard => {}
            },
            Node::Statement(statement) => match statement {
                Stmt::VarDecl {
                    name, ty, value, ..
                } => {
                    pending.push(Node::Text(name));
                    pending.extend(ty.iter().map(Node::AstType));
                    pending.push(Node::Expression(value));
                }
                Stmt::Assign { name, value, .. } => {
                    pending.push(Node::Text(name));
                    pending.push(Node::Expression(value));
                }
                Stmt::AssignTarget { target, value, .. }
                | Stmt::CompoundAssign { target, value, .. } => {
                    pending.push(Node::Target(target));
                    pending.push(Node::Expression(value));
                }
                Stmt::ObjectDestructureAssign {
                    bindings,
                    rest,
                    value,
                    ..
                } => {
                    pending.push(Node::Expression(value));
                    for binding in bindings {
                        pending.push(Node::Text(&binding.field));
                        pending.extend(binding.local.iter().map(|name| Node::Text(name)));
                        pending.extend(binding.default.iter().map(Node::Expression));
                    }
                    if let Some(rest) = rest {
                        pending.extend(rest.local.iter().map(|name| Node::Text(name)));
                    }
                }
                Stmt::DestructureAssign { names, values, .. } => {
                    pending.extend(names.iter().flatten().map(|name| Node::Text(name)));
                    pending.extend(values.iter().map(Node::Expression));
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    pending.push(Node::Expression(condition));
                    pending.extend(then_branch.iter().chain(else_branch).map(Node::Statement));
                }
                Stmt::While {
                    condition, body, ..
                } => {
                    pending.push(Node::Expression(condition));
                    pending.extend(body.iter().map(Node::Statement));
                }
                Stmt::For {
                    name,
                    iterable,
                    body,
                    ..
                } => {
                    pending.push(Node::Text(name));
                    pending.push(Node::Expression(iterable));
                    pending.extend(body.iter().map(Node::Statement));
                }
                Stmt::Try {
                    body,
                    catch_name,
                    catch_body,
                    finally_body,
                    ..
                } => {
                    pending.extend(catch_name.iter().map(|name| Node::Text(name)));
                    pending.extend(
                        body.iter()
                            .chain(catch_body)
                            .chain(finally_body)
                            .map(Node::Statement),
                    );
                }
                Stmt::Function(function) => pending.push(Node::Function(function)),
                Stmt::Return { value, .. } => pending.extend(value.iter().map(Node::Expression)),
                Stmt::Fail { value, .. }
                | Stmt::Panic { value, .. }
                | Stmt::Print { value, .. } => pending.push(Node::Expression(value)),
                Stmt::Expr { expr, .. } => pending.push(Node::Expression(expr)),
                Stmt::Break { .. } | Stmt::Continue { .. } => {}
            },
            Node::Expression(expression) => match &expression.kind {
                ExprKind::Literal(Literal::String(text) | Literal::TemplateString(text))
                | ExprKind::Variable(text) => pending.push(Node::Text(text)),
                ExprKind::Literal(_) => {}
                ExprKind::Unary { expr, .. }
                | ExprKind::Await(expr)
                | ExprKind::TryUnwrap { expr } => pending.push(Node::Expression(expr)),
                ExprKind::Field { target, name } | ExprKind::OptionalField { target, name } => {
                    pending.push(Node::Expression(target));
                    pending.push(Node::Text(name));
                }
                ExprKind::Binary { left, right, .. } => {
                    pending.push(Node::Expression(left));
                    pending.push(Node::Expression(right));
                }
                ExprKind::Index { target, index } => {
                    pending.push(Node::Expression(target));
                    pending.push(Node::Expression(index));
                }
                ExprKind::Call { callee, args } => {
                    pending.push(Node::Expression(callee));
                    pending.extend(args.iter().map(Node::Expression));
                }
                ExprKind::Array(values) => pending.extend(values.iter().map(Node::Expression)),
                ExprKind::StructLiteral { name, fields } => {
                    pending.push(Node::Text(name));
                    for (name, value) in fields {
                        pending.push(Node::Text(name));
                        pending.push(Node::Expression(value));
                    }
                }
                ExprKind::ObjectLiteral { fields } => {
                    for (name, value) in fields {
                        pending.push(Node::Text(name));
                        pending.push(Node::Expression(value));
                    }
                }
                ExprKind::Match { value, arms } => {
                    pending.push(Node::Expression(value));
                    for arm in arms {
                        pending.push(Node::Pattern(&arm.pattern));
                        pending.extend(arm.guard.iter().map(Node::Expression));
                        pending.push(Node::Expression(&arm.value));
                    }
                }
                ExprKind::Function {
                    params,
                    return_type,
                    body,
                } => {
                    pending.extend(params.iter().map(Node::FunctionParameter));
                    pending.extend(return_type.iter().map(Node::AstType));
                    pending.extend(body.iter().map(Node::Statement));
                }
            },
        }
    }
    if nodes > node_limit || (!scanning && bytes > MAX_INSTANCE_BODY_BYTES / 2) {
        return Err(limit(span, "AST/type payload bytes or nodes"));
    }
    Ok((nodes, bytes, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(body: Vec<Stmt>) -> FunctionType {
        FunctionType {
            type_params: vec!["T".into()],
            params: vec![Type::Generic("T".into())],
            value_params: vec![],
            return_type: None,
            returns: Type::Null,
            body,
            body_id: 1,
            is_async: false,
        }
    }

    #[test]
    fn generic_budget_counts_assignment_indices_destructure_defaults_and_type_names() {
        let span = Span::default();
        let huge = "x".repeat(MAX_INSTANCE_BODY_BYTES / 2);
        let string = || Expr::new(ExprKind::Literal(Literal::String(huge.clone())), span);
        let statements = [
            Stmt::AssignTarget {
                target: AssignTarget::Index {
                    target: Expr::new(ExprKind::Variable("items".into()), span),
                    index: string(),
                },
                value: Expr::new(ExprKind::Literal(Literal::Int(1)), span),
                span,
            },
            Stmt::ObjectDestructureAssign {
                bindings: vec![ObjectDestructureBinding {
                    field: "field".into(),
                    local: Some("field".into()),
                    default: Some(string()),
                    span,
                }],
                rest: None,
                value: Expr::new(ExprKind::Variable("object".into()), span),
                span,
            },
            Stmt::VarDecl {
                name: "value".into(),
                mutable: false,
                ty: Some(TypeName::Custom(huge)),
                value: Expr::new(ExprKind::Literal(Literal::Null), span),
                span,
            },
        ];
        for statement in statements {
            assert!(instance_cost(
                &function(vec![statement]),
                &HashMap::new(),
                &[Type::Int],
                span
            )
            .is_err());
        }
    }

    #[test]
    fn generic_budget_counts_callback_body_payload_before_request_clone() {
        let callback = Type::FunctionValue {
            params: vec![],
            return_type: Some(Box::new(Type::Null)),
            body: vec![Stmt::Print {
                value: Expr::new(
                    ExprKind::Literal(Literal::String("x".repeat(MAX_INSTANCE_BODY_BYTES / 2))),
                    Span::default(),
                ),
                span: Span::default(),
            }],
            body_id: Some(2),
            is_async: false,
        };
        assert!(instance_cost(
            &function(vec![]),
            &HashMap::new(),
            &[callback],
            Span::default()
        )
        .is_err());
    }
}

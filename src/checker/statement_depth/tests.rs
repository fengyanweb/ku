use super::*;
use crate::{
    checker::{native_specialization_plan, Checker},
    error::DiagnosticId,
    span::Position,
};

fn mark(offset: usize) -> Span {
    Span::point(Position::new(1, offset + 1, offset))
}

fn literal() -> Expr {
    Expr::new(ExprKind::Literal(Literal::Bool(true)), mark(0))
}

fn function(name: &str, body: Vec<Stmt>) -> FnDecl {
    FnDecl {
        name: name.into(),
        is_async: false,
        type_params: Vec::new(),
        params: Vec::new(),
        return_type: None,
        body,
        span: mark(0),
    }
}

fn program(body: Vec<Stmt>) -> Program {
    Program {
        items: vec![Item::Function(function("main", body))],
    }
}

fn expression_statement(expr: Expr) -> Stmt {
    Stmt::Expr {
        expr,
        span: mark(0),
    }
}

fn owner(kind: &str, body: Vec<Stmt>, span: Span) -> Stmt {
    match kind {
        "if_then" => Stmt::If {
            condition: literal(),
            then_branch: body,
            else_branch: Vec::new(),
            span,
        },
        "if_else" => Stmt::If {
            condition: literal(),
            then_branch: Vec::new(),
            else_branch: body,
            span,
        },
        "while" => Stmt::While {
            condition: literal(),
            body,
            span,
        },
        "for" => Stmt::For {
            name: "n".into(),
            iterable: literal(),
            body,
            span,
        },
        "try" | "catch" | "finally" => {
            let (body, catch_body, finally_body) = match kind {
                "try" => (body, Vec::new(), Vec::new()),
                "catch" => (Vec::new(), body, Vec::new()),
                _ => (Vec::new(), Vec::new(), body),
            };
            Stmt::Try {
                body,
                catch_name: Some("err".into()),
                catch_body,
                finally_body,
                span,
            }
        }
        "local_fn" | "async_fn" => {
            let mut function = function("local", body);
            function.is_async = kind == "async_fn";
            function.span = span;
            Stmt::Function(function)
        }
        "closure" => expression_statement(Expr::new(
            ExprKind::Function {
                params: Vec::new(),
                return_type: None,
                body,
            },
            span,
        )),
        _ => panic!("unknown body owner: {kind}"),
    }
}

fn nested(kind: &str, count: usize, span: Span) -> Vec<Stmt> {
    let mut body = Vec::new();
    for _ in 0..count {
        body = vec![owner(kind, body, span)];
    }
    body
}

fn deep_expression(span: Span) -> Expr {
    expression_body(32, span)
}

fn expression_body(if_count: usize, span: Span) -> Expr {
    Expr::new(
        ExprKind::Function {
            params: Vec::new(),
            return_type: None,
            body: nested("if_then", if_count, span),
        },
        span,
    )
}

fn assert_limit(result: KuResult<()>, span: Span) {
    let error = result.expect_err("33 body owners must be rejected");
    assert_eq!(error.diagnostic_id(), DiagnosticId::RuntimeError);
    assert_eq!(
        error.message,
        "maximum check depth exceeded; statement body is too deeply nested"
    );
    assert_eq!(error.span, span);
}

#[test]
fn statement_depth_raw_all_owners_accept_32_and_reject_33() {
    for kind in [
        "if_then", "if_else", "while", "for", "try", "catch", "finally", "local_fn", "async_fn",
        "closure",
    ] {
        check(&program(nested(kind, 32, mark(71)))).unwrap();
        assert_limit(check(&program(nested(kind, 33, mark(71)))), mark(71));
    }
}

#[test]
fn statement_depth_raw_siblings_roots_and_mixed_owners_do_not_reset_or_accumulate() {
    let mut body = nested("if_then", 32, mark(1));
    body.extend(nested("closure", 32, mark(2)));
    let mut value = program(body);
    value.items.push(Item::Function(function(
        "other",
        nested("while", 32, mark(3)),
    )));
    check(&value).unwrap();
    let mut body = nested("if_then", 31, mark(4));
    body = vec![owner("closure", body, mark(5))];
    check(&program(body)).unwrap();
    let body = vec![owner("closure", nested("if_then", 32, mark(4)), mark(5))];
    assert_limit(check(&program(body)), mark(4));
}

#[test]
fn statement_depth_preflight_precedes_declarations_and_native_generic_entry() {
    let mut value = program(nested("if_then", 33, mark(72)));
    value
        .items
        .insert(0, Item::Function(function("main", Vec::new())));
    assert_limit(Checker::new().check(&value), mark(72));
    let error = native_specialization_plan(&value)
        .err()
        .expect("preflight must run");
    assert_eq!(
        error.message,
        "maximum check depth exceeded; statement body is too deeply nested"
    );
    assert_eq!(error.span, mark(72));
}

#[test]
fn statement_depth_raw_expression_routes_do_not_hide_closure_bodies() {
    for route in [
        "direct",
        "unary",
        "await",
        "unwrap",
        "left",
        "right",
        "callee",
        "argument",
        "array",
        "index_target",
        "index_value",
        "field",
        "optional_field",
        "struct",
        "object",
        "match_value",
        "match_guard",
        "match_arm",
    ] {
        let payload = deep_expression(mark(73));
        let kind = match route {
            "direct" => payload.kind,
            "unary" => ExprKind::Unary {
                op: UnaryOp::Not,
                expr: Box::new(payload),
            },
            "await" => ExprKind::Await(Box::new(payload)),
            "unwrap" => ExprKind::TryUnwrap {
                expr: Box::new(payload),
            },
            "left" => ExprKind::Binary {
                left: Box::new(payload),
                op: BinaryOp::Add,
                right: Box::new(literal()),
            },
            "right" => ExprKind::Binary {
                left: Box::new(literal()),
                op: BinaryOp::Add,
                right: Box::new(payload),
            },
            "callee" => ExprKind::Call {
                callee: Box::new(payload),
                args: Vec::new(),
            },
            "argument" => ExprKind::Call {
                callee: Box::new(literal()),
                args: vec![literal(), payload],
            },
            "array" => ExprKind::Array(vec![literal(), payload]),
            "index_target" => ExprKind::Index {
                target: Box::new(payload),
                index: Box::new(literal()),
            },
            "index_value" => ExprKind::Index {
                target: Box::new(literal()),
                index: Box::new(payload),
            },
            "field" => ExprKind::Field {
                target: Box::new(payload),
                name: "field".into(),
            },
            "optional_field" => ExprKind::OptionalField {
                target: Box::new(payload),
                name: "field".into(),
            },
            "struct" => ExprKind::StructLiteral {
                name: "Unknown".into(),
                fields: vec![("first".into(), literal()), ("last".into(), payload)],
            },
            "object" => ExprKind::ObjectLiteral {
                fields: vec![("first".into(), literal()), ("last".into(), payload)],
            },
            "match_value" => ExprKind::Match {
                value: Box::new(payload),
                arms: Vec::new(),
            },
            "match_guard" => ExprKind::Match {
                value: Box::new(literal()),
                arms: vec![MatchArm {
                    pattern: MatchPattern::Wildcard,
                    guard: Some(payload),
                    value: literal(),
                    span: mark(0),
                }],
            },
            "match_arm" => ExprKind::Match {
                value: Box::new(literal()),
                arms: vec![MatchArm {
                    pattern: MatchPattern::Wildcard,
                    guard: None,
                    value: payload,
                    span: mark(0),
                }],
            },
            _ => unreachable!(),
        };
        let value = program(vec![expression_statement(Expr::new(kind, mark(73)))]);
        assert_limit(check(&value), mark(73));
    }
}

#[test]
fn statement_depth_raw_statement_and_assignment_routes_are_exhaustive() {
    for route in [
        "var",
        "assign",
        "target_value",
        "compound_value",
        "target_index",
        "target_base",
        "target_field",
        "compound_target",
        "destructure",
        "object_value",
        "object_default",
        "fail",
        "panic",
        "print",
        "return",
        "expression",
        "if_condition",
        "while_condition",
        "for_iterable",
    ] {
        let payload = if matches!(route, "if_condition" | "while_condition" | "for_iterable") {
            expression_body(31, mark(74))
        } else {
            deep_expression(mark(74))
        };
        let span = mark(0);
        let stmt = match route {
            "var" => Stmt::VarDecl {
                name: "value".into(),
                mutable: false,
                ty: None,
                value: payload,
                span,
            },
            "assign" => Stmt::Assign {
                name: "value".into(),
                value: payload,
                span,
            },
            "target_value" => Stmt::AssignTarget {
                target: AssignTarget::Variable("value".into()),
                value: payload,
                span,
            },
            "compound_value" => Stmt::CompoundAssign {
                target: AssignTarget::Variable("value".into()),
                op: BinaryOp::Add,
                value: payload,
                span,
            },
            "target_index" => Stmt::AssignTarget {
                target: AssignTarget::Index {
                    target: literal(),
                    index: payload,
                },
                value: literal(),
                span,
            },
            "target_base" => Stmt::AssignTarget {
                target: AssignTarget::Index {
                    target: payload,
                    index: literal(),
                },
                value: literal(),
                span,
            },
            "target_field" => Stmt::AssignTarget {
                target: AssignTarget::Field {
                    target: payload,
                    name: "field".into(),
                },
                value: literal(),
                span,
            },
            "compound_target" => Stmt::CompoundAssign {
                target: AssignTarget::Field {
                    target: payload,
                    name: "field".into(),
                },
                op: BinaryOp::Add,
                value: literal(),
                span,
            },
            "destructure" => Stmt::DestructureAssign {
                names: vec![None, None],
                values: vec![literal(), payload],
                span,
            },
            "object_value" => Stmt::ObjectDestructureAssign {
                bindings: Vec::new(),
                rest: None,
                value: payload,
                span,
            },
            "object_default" => Stmt::ObjectDestructureAssign {
                bindings: vec![ObjectDestructureBinding {
                    field: "field".into(),
                    local: None,
                    default: Some(payload),
                    span,
                }],
                rest: None,
                value: literal(),
                span,
            },
            "fail" => Stmt::Fail {
                value: payload,
                span,
            },
            "panic" => Stmt::Panic {
                value: payload,
                span,
            },
            "print" => Stmt::Print {
                value: payload,
                span,
            },
            "return" => Stmt::Return {
                value: Some(payload),
                span,
            },
            "expression" => expression_statement(payload),
            "if_condition" => Stmt::If {
                condition: payload,
                then_branch: Vec::new(),
                else_branch: Vec::new(),
                span,
            },
            "while_condition" => Stmt::While {
                condition: payload,
                body: Vec::new(),
                span,
            },
            "for_iterable" => Stmt::For {
                name: "n".into(),
                iterable: payload,
                body: Vec::new(),
                span,
            },
            _ => unreachable!(),
        };
        assert_limit(check(&program(vec![stmt])), mark(74));
    }
}

#[test]
fn statement_depth_raw_error_order_is_source_order_even_after_return() {
    let value = program(vec![
        Stmt::Return {
            value: None,
            span: mark(0),
        },
        Stmt::If {
            condition: Expr::new(ExprKind::Literal(Literal::Bool(false)), mark(0)),
            then_branch: vec![expression_statement(expression_body(31, mark(81)))],
            else_branch: vec![expression_statement(expression_body(31, mark(82)))],
            span: mark(0),
        },
    ]);
    assert_limit(check(&value), mark(81));
    let value = program(vec![Stmt::ObjectDestructureAssign {
        bindings: vec![ObjectDestructureBinding {
            field: "field".into(),
            local: None,
            default: Some(deep_expression(mark(83))),
            span: mark(0),
        }],
        rest: None,
        value: deep_expression(mark(84)),
        span: mark(0),
    }]);
    assert_limit(check(&value), mark(83));
}

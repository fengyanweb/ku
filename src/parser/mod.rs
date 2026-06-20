use crate::ast::*;
use crate::error::{KuError, KuResult};
use crate::span::{Position, Span};
use crate::token::{Token, TokenKind};

const MAX_PARSE_DEPTH: usize = 32;
const MAX_TOKENS: usize = 100_000;

pub struct Parser {
    tokens: Vec<Token>,
    current: usize,
    parse_depth: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            current: 0,
            parse_depth: 0,
        }
    }

    pub fn parse(&mut self) -> KuResult<Program> {
        self.check_token_limit()?;
        let mut items = Vec::new();
        while !self.check(&TokenKind::Eof) {
            items.push(self.item()?);
        }
        Ok(Program { items })
    }

    pub fn parse_program(&mut self) -> KuResult<Program> {
        self.parse()
    }

    pub fn parse_expression_only(&mut self) -> KuResult<Expr> {
        self.check_token_limit()?;
        let expr = self.expression()?;
        self.consume(&TokenKind::Eof, "expected end of expression")?;
        Ok(expr)
    }

    fn check_token_limit(&self) -> KuResult<()> {
        if self.tokens.len() > MAX_TOKENS {
            Err(KuError::parse(
                "too many tokens; input is too large for Ku",
                self.peek().span,
            ))
        } else {
            Ok(())
        }
    }

    fn item(&mut self) -> KuResult<Item> {
        if self.check(&TokenKind::Import) {
            return Ok(Item::Import(self.import_decl()?));
        }
        if self.match_kind(&TokenKind::Async) {
            return Ok(Item::Function(self.function(true)?));
        }
        if self.check(&TokenKind::Fn) {
            return Ok(Item::Function(self.function(false)?));
        }
        if self.check(&TokenKind::Struct) {
            return Ok(Item::Struct(self.struct_decl()?));
        }
        if self.check(&TokenKind::Enum) {
            return Ok(Item::Enum(self.enum_decl()?));
        }
        if self.check(&TokenKind::Module) {
            return Ok(Item::Module(self.module_decl()?));
        }
        if self.check(&TokenKind::Return) {
            return Err(KuError::parse("return outside function", self.peek().span));
        }
        Err(KuError::parse(
            "expected top-level item: import, fn, struct, enum, or module",
            self.peek().span,
        ))
    }

    fn import_decl(&mut self) -> KuResult<ImportDecl> {
        let start = self
            .consume(&TokenKind::Import, "expected 'import'")?
            .span
            .start;
        let kind = if self.match_kind(&TokenKind::LBrace) {
            let mut names = Vec::new();
            if self.check(&TokenKind::RBrace) {
                return Err(KuError::parse(
                    "expected imported name inside '{ }'",
                    self.peek().span,
                ));
            }
            loop {
                let (name, span) = self.consume_ident("expected imported name")?;
                if !is_exported_name(&name) {
                    return Err(KuError::parse(
                        format!("imported name '{name}' is not exported; exported names must start with uppercase"),
                        span,
                    ));
                }
                let alias = if self.match_ident_text("as") {
                    let (alias, alias_span) = self.consume_ident("expected import alias")?;
                    if !is_valid_namespace(&alias) {
                        return Err(KuError::parse("expected import alias", alias_span));
                    }
                    Some(alias)
                } else {
                    None
                };
                names.push(ImportName {
                    source: name,
                    alias,
                    span,
                });
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
            self.consume(&TokenKind::RBrace, "expected '}' after imported names")?;
            self.consume(&TokenKind::From, "expected 'from' after imported names")?;
            ImportKind::Named(names)
        } else if self.check(&TokenKind::String(String::new())) {
            ImportKind::Glob
        } else if self.check(&TokenKind::From) {
            return Err(KuError::parse(
                "use 'import \"./xxx.ku\"' instead of 'import from \"./xxx.ku\"'",
                self.peek().span,
            ));
        } else {
            let (namespace, span) = self.consume_ident("expected import namespace or path")?;
            if !is_valid_namespace(&namespace) {
                return Err(KuError::parse("expected import namespace", span));
            }
            self.consume(&TokenKind::From, "expected 'from' after import namespace")?;
            ImportKind::Namespace(namespace)
        };
        let (path, path_span) = self.consume_string("expected import path string")?;
        self.optional_semicolon();
        Ok(ImportDecl {
            kind,
            path,
            span: Span::new(start, path_span.end),
        })
    }

    fn function(&mut self, is_async: bool) -> KuResult<FnDecl> {
        let start = if is_async {
            let start = self.previous().span.start;
            self.consume(&TokenKind::Fn, "expected 'fn' after 'async'")?;
            start
        } else {
            self.consume(&TokenKind::Fn, "expected 'fn'")?.span.start
        };
        let (name, _) = self.consume_ident("expected function name")?;
        let type_params = self.type_params()?;
        self.consume(&TokenKind::LParen, "expected '(' after function name")?;

        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                let (param_name, param_span) = self.consume_ident("expected parameter name")?;
                let ty = if self.match_kind(&TokenKind::Colon) {
                    Some(self.type_name()?)
                } else {
                    None
                };
                params.push(Param {
                    name: param_name,
                    ty,
                    span: param_span,
                });
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }

        self.consume(&TokenKind::RParen, "expected ')' after parameters")?;
        let return_type = if self.match_kind(&TokenKind::Colon) {
            Some(self.type_name()?)
        } else {
            None
        };
        let (body, body_span) = self.block()?;
        Ok(FnDecl {
            name,
            is_async,
            type_params,
            params,
            return_type,
            body,
            span: Span::new(start, body_span.end),
        })
    }

    fn type_params(&mut self) -> KuResult<Vec<String>> {
        if !self.match_kind(&TokenKind::Less) {
            return Ok(Vec::new());
        }
        let mut params = Vec::new();
        loop {
            let (name, span) = self.consume_ident("expected generic type parameter")?;
            if params.contains(&name) {
                return Err(KuError::parse(
                    format!("duplicate generic type parameter '{name}'"),
                    span,
                ));
            }
            params.push(name);
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.consume(
            &TokenKind::Greater,
            "expected '>' after generic type parameters",
        )?;
        Ok(params)
    }

    fn struct_decl(&mut self) -> KuResult<StructDecl> {
        let start = self
            .consume(&TokenKind::Struct, "expected 'struct'")?
            .span
            .start;
        let (name, _) = self.consume_ident("expected struct name")?;
        self.consume(&TokenKind::LBrace, "expected '{' after struct name")?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            let (field_name, field_span) = self.consume_ident("expected struct field name")?;
            self.consume(&TokenKind::Colon, "expected ':' after struct field name")?;
            let ty = self.type_name()?;
            fields.push(Param {
                name: field_name,
                ty: Some(ty),
                span: field_span,
            });
            self.match_kind(&TokenKind::Comma);
        }
        let end = self
            .consume(&TokenKind::RBrace, "expected '}' after struct fields")?
            .span
            .end;
        Ok(StructDecl {
            name,
            fields,
            span: Span::new(start, end),
        })
    }

    fn enum_decl(&mut self) -> KuResult<EnumDecl> {
        let start = self
            .consume(&TokenKind::Enum, "expected 'enum'")?
            .span
            .start;
        let (name, _) = self.consume_ident("expected enum name")?;
        self.consume(&TokenKind::LBrace, "expected '{' after enum name")?;
        let mut variants = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            let (variant_name, variant_span) = self.consume_ident("expected enum variant name")?;
            let mut fields = Vec::new();
            let mut end = variant_span.end;
            if self.match_kind(&TokenKind::LParen) {
                if !self.check(&TokenKind::RParen) {
                    loop {
                        let (field_name, field_span) =
                            self.consume_ident("expected enum variant field name")?;
                        self.consume(&TokenKind::Colon, "expected ':' after variant field name")?;
                        let ty = self.type_name()?;
                        fields.push(Param {
                            name: field_name,
                            ty: Some(ty),
                            span: field_span,
                        });
                        if !self.match_kind(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                end = self
                    .consume(&TokenKind::RParen, "expected ')' after enum variant fields")?
                    .span
                    .end;
            }
            variants.push(EnumVariant {
                name: variant_name,
                fields,
                span: Span::new(variant_span.start, end),
            });
            self.match_kind(&TokenKind::Comma);
        }
        let end = self
            .consume(&TokenKind::RBrace, "expected '}' after enum variants")?
            .span
            .end;
        Ok(EnumDecl {
            name,
            variants,
            span: Span::new(start, end),
        })
    }

    fn module_decl(&mut self) -> KuResult<ModuleDecl> {
        let start = self
            .consume(&TokenKind::Module, "expected 'module'")?
            .span
            .start;
        let (name, span) = self.consume_ident("expected module name")?;
        self.optional_semicolon();
        Ok(ModuleDecl {
            name,
            span: Span::new(start, span.end),
        })
    }

    fn type_name(&mut self) -> KuResult<TypeName> {
        let first = self.type_atom()?;
        let mut types = vec![self.finish_type_name(first)?];
        while self.match_kind(&TokenKind::Pipe) {
            let next = self.type_atom()?;
            types.push(self.finish_type_name(next)?);
        }
        if types.len() == 1 {
            Ok(types.remove(0))
        } else {
            Ok(TypeName::Union(types))
        }
    }

    fn type_atom(&mut self) -> KuResult<TypeName> {
        if self.match_kind(&TokenKind::LBracket) {
            let inner = self.type_name()?;
            self.consume(&TokenKind::RBracket, "expected ']' after array type")?;
            return Ok(TypeName::Array(Box::new(inner)));
        }
        let token = self.advance().clone();
        let ty = match token.kind {
            TokenKind::Ident(name) => match name.as_str() {
                "int" => TypeName::Int,
                "float" => TypeName::Float,
                "bool" => TypeName::Bool,
                "str" => TypeName::String,
                "string" | "nil" => return Err(KuError::parse("expected type name", token.span)),
                _ => {
                    let mut name = name;
                    while self.match_kind(&TokenKind::Dot) {
                        let (part, _) = self.consume_ident("expected type name after '.'")?;
                        name.push('.');
                        name.push_str(&part);
                    }
                    TypeName::Custom(name)
                }
            },
            TokenKind::Null => TypeName::Null,
            _ => return Err(KuError::parse("expected type name", token.span)),
        };
        Ok(ty)
    }

    fn finish_type_name(&mut self, ty: TypeName) -> KuResult<TypeName> {
        if self.match_kind(&TokenKind::Bang) {
            Ok(TypeName::Result(Box::new(ty)))
        } else {
            Ok(ty)
        }
    }

    fn block(&mut self) -> KuResult<(Vec<Stmt>, Span)> {
        let start = self.consume(&TokenKind::LBrace, "expected '{'")?.span.start;
        let mut statements = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            statements.push(self.statement()?);
        }
        let end = self
            .consume(&TokenKind::RBrace, "expected '}' after block")?
            .span
            .end;
        Ok((statements, Span::new(start, end)))
    }

    fn statement(&mut self) -> KuResult<Stmt> {
        if self.match_kind(&TokenKind::Let) {
            return Err(KuError::parse(
                "'let' is not supported in Ku; use 'name = value' or 'name:type = value'",
                self.previous().span,
            ));
        }
        if self.match_kind(&TokenKind::If) {
            let stmt = self.if_statement()?;
            self.optional_semicolon();
            return Ok(stmt);
        }
        if self.match_kind(&TokenKind::While) {
            let stmt = self.while_statement()?;
            self.optional_semicolon();
            return Ok(stmt);
        }
        if self.match_kind(&TokenKind::For) {
            let stmt = self.for_statement()?;
            self.optional_semicolon();
            return Ok(stmt);
        }
        if self.match_kind(&TokenKind::Break) {
            let span = self.previous().span;
            self.optional_semicolon();
            return Ok(Stmt::Break { span });
        }
        if self.match_kind(&TokenKind::Continue) {
            let span = self.previous().span;
            self.optional_semicolon();
            return Ok(Stmt::Continue { span });
        }
        if self.match_kind(&TokenKind::Try) {
            return self.try_statement();
        }
        if self.match_kind(&TokenKind::Async) {
            let function = self.function(true)?;
            self.optional_semicolon();
            return Ok(Stmt::Function(function));
        }
        if self.check(&TokenKind::Fn) {
            let function = self.function(false)?;
            self.optional_semicolon();
            return Ok(Stmt::Function(function));
        }
        if self.match_kind(&TokenKind::Fail) {
            return self.fail_statement();
        }
        if self.match_kind(&TokenKind::Panic) {
            return self.panic_statement();
        }
        if self.match_kind(&TokenKind::Return) {
            return self.return_statement();
        }
        if self.match_kind(&TokenKind::Print) {
            return self.print_statement();
        }

        self.expression_or_assignment_statement()
    }

    fn if_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        self.consume(&TokenKind::LParen, "expected '(' after 'if'")?;
        let condition = self.expression()?;
        self.consume(&TokenKind::RParen, "expected ')' after if condition")?;
        let (then_branch, then_span) = self.block()?;
        let (else_branch, end) = if self.match_kind(&TokenKind::Else) {
            if self.match_kind(&TokenKind::If) {
                let nested = self.if_statement()?;
                let span = stmt_span(&nested);
                (vec![nested], span.end)
            } else {
                let (body, span) = self.block()?;
                (body, span.end)
            }
        } else {
            (Vec::new(), then_span.end)
        };
        Ok(Stmt::If {
            condition,
            then_branch,
            else_branch,
            span: Span::new(start, end),
        })
    }

    fn while_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        self.consume(&TokenKind::LParen, "expected '(' after 'while'")?;
        let condition = self.expression()?;
        self.consume(&TokenKind::RParen, "expected ')' after while condition")?;
        let (body, body_span) = self.block()?;
        Ok(Stmt::While {
            condition,
            body,
            span: Span::new(start, body_span.end),
        })
    }

    fn for_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        let (name, _) = self.consume_ident("expected loop variable after 'for'")?;
        self.consume(&TokenKind::In, "expected 'in' after loop variable")?;
        let iterable = self.expression()?;
        let (body, body_span) = self.block()?;
        Ok(Stmt::For {
            name,
            iterable,
            body,
            span: Span::new(start, body_span.end),
        })
    }

    fn return_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        let value = if self.check(&TokenKind::RBrace) || self.check(&TokenKind::Semicolon) {
            None
        } else {
            Some(self.expression()?)
        };
        self.optional_semicolon();
        let end = value
            .as_ref()
            .map_or(self.previous().span.end, |expr| expr.span.end);
        Ok(Stmt::Return {
            value,
            span: Span::new(start, end),
        })
    }

    fn fail_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        let value = self.expression()?;
        self.optional_semicolon();
        let end = value.span.end;
        Ok(Stmt::Fail {
            value,
            span: Span::new(start, end),
        })
    }

    fn panic_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        let value = self.expression()?;
        self.optional_semicolon();
        let end = value.span.end;
        Ok(Stmt::Panic {
            value,
            span: Span::new(start, end),
        })
    }

    fn try_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        let (body, body_span) = self.block()?;
        let mut catch_name = None;
        let mut catch_body = Vec::new();
        let mut finally_body = Vec::new();
        let mut end = body_span.end;
        if self.match_kind(&TokenKind::Catch) {
            self.consume(&TokenKind::LParen, "expected '(' after 'catch'")?;
            let (name, _) = self.consume_ident("expected catch error name")?;
            self.consume(&TokenKind::RParen, "expected ')' after catch error name")?;
            let (body, span) = self.block()?;
            catch_name = Some(name);
            catch_body = body;
            end = span.end;
        }
        if self.match_kind(&TokenKind::Finally) {
            let (body, span) = self.block()?;
            finally_body = body;
            end = span.end;
        }
        if catch_name.is_none() && finally_body.is_empty() {
            return Err(KuError::parse(
                "try requires catch or finally",
                Span::new(start, end),
            ));
        }
        Ok(Stmt::Try {
            body,
            catch_name,
            catch_body,
            finally_body,
            span: Span::new(start, end),
        })
    }

    fn print_statement(&mut self) -> KuResult<Stmt> {
        let start = self.previous().span.start;
        let value = if self.match_kind(&TokenKind::LParen) {
            let expr = self.expression()?;
            self.consume(&TokenKind::RParen, "expected ')' after print argument")?;
            expr
        } else {
            self.expression()?
        };
        self.optional_semicolon();
        let end = value.span.end;
        Ok(Stmt::Print {
            value,
            span: Span::new(start, end),
        })
    }

    fn expression_or_assignment_statement(&mut self) -> KuResult<Stmt> {
        if self.is_destructure_assignment_start() {
            return self.destructure_assignment_statement();
        }
        if let TokenKind::Ident(name) = self.peek().kind.clone() {
            if self.peek_next_kind() == Some(&TokenKind::Colon) {
                let start = self.advance().span.start;
                self.advance();
                let ty = self.type_name()?;
                let value = if self.match_kind(&TokenKind::Equal) {
                    self.expression()?
                } else {
                    default_expr(&ty, Span::point(start))
                };
                self.optional_semicolon();
                let end = value.span.end;
                return Ok(Stmt::VarDecl {
                    name,
                    mutable: true,
                    ty: Some(ty),
                    value,
                    span: Span::new(start, end),
                });
            }
        }

        let expr = self.expression()?;
        if self.match_kind(&TokenKind::Equal) {
            let start = expr.span.start;
            let target = assign_target_from_expr(expr)?;
            let value = self.expression()?;
            self.optional_semicolon();
            let end = value.span.end;
            return match target {
                AssignTarget::Variable(name) => Ok(Stmt::Assign {
                    name,
                    value,
                    span: Span::new(start, end),
                }),
                target => Ok(Stmt::AssignTarget {
                    target,
                    value,
                    span: Span::new(start, end),
                }),
            };
        }
        if self.match_kind(&TokenKind::PlusPlus) {
            return self.increment_statement(expr, BinaryOp::Add);
        }
        if self.match_kind(&TokenKind::MinusMinus) {
            return self.increment_statement(expr, BinaryOp::Subtract);
        }
        self.optional_semicolon();
        let span = expr.span;
        Ok(Stmt::Expr { expr, span })
    }

    fn destructure_assignment_statement(&mut self) -> KuResult<Stmt> {
        let start = self.peek().span.start;
        let mut names = Vec::new();
        loop {
            let (name, _) = self.consume_ident("expected destructuring target")?;
            names.push(if name == "_" { None } else { Some(name) });
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.consume(
            &TokenKind::Equal,
            "expected '=' after destructuring targets",
        )?;
        let mut values = Vec::new();
        loop {
            values.push(self.expression()?);
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.optional_semicolon();
        let end = values
            .last()
            .map(|value| value.span.end)
            .unwrap_or_else(|| Span::point(start).end);
        Ok(Stmt::DestructureAssign {
            names,
            values,
            span: Span::new(start, end),
        })
    }

    fn is_destructure_assignment_start(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Ident(_))
            && matches!(
                self.tokens.get(self.current + 1).map(|token| &token.kind),
                Some(TokenKind::Comma)
            )
    }

    fn increment_statement(&mut self, expr: Expr, op: BinaryOp) -> KuResult<Stmt> {
        let start = expr.span.start;
        let end = self.previous().span.end;
        let target = assign_target_from_expr(expr.clone())?;
        let one = Expr::new(ExprKind::Literal(Literal::Int(1)), Span::new(start, end));
        let value = Expr::new(
            ExprKind::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(one),
            },
            Span::new(start, end),
        );
        self.optional_semicolon();
        match target {
            AssignTarget::Variable(name) => Ok(Stmt::Assign {
                name,
                value,
                span: Span::new(start, end),
            }),
            target => Ok(Stmt::AssignTarget {
                target,
                value,
                span: Span::new(start, end),
            }),
        }
    }

    fn expression(&mut self) -> KuResult<Expr> {
        self.enter_parse_depth()?;
        let result = self.or();
        self.leave_parse_depth();
        result
    }

    fn or(&mut self) -> KuResult<Expr> {
        let mut expr = self.and()?;
        while self.match_kind(&TokenKind::OrOr) {
            let right = self.and()?;
            let span = expr.span.merge(right.span);
            expr = Expr::new(
                ExprKind::Binary {
                    left: Box::new(expr),
                    op: BinaryOp::Or,
                    right: Box::new(right),
                },
                span,
            );
        }
        Ok(expr)
    }

    fn and(&mut self) -> KuResult<Expr> {
        let mut expr = self.equality()?;
        while self.match_kind(&TokenKind::AndAnd) {
            let right = self.equality()?;
            let span = expr.span.merge(right.span);
            expr = Expr::new(
                ExprKind::Binary {
                    left: Box::new(expr),
                    op: BinaryOp::And,
                    right: Box::new(right),
                },
                span,
            );
        }
        Ok(expr)
    }

    fn equality(&mut self) -> KuResult<Expr> {
        let mut expr = self.comparison()?;
        while self.match_any(&[TokenKind::BangEqual, TokenKind::EqualEqual]) {
            let op = match self.previous().kind {
                TokenKind::BangEqual => BinaryOp::NotEqual,
                _ => BinaryOp::Equal,
            };
            let right = self.comparison()?;
            let span = expr.span.merge(right.span);
            expr = Expr::new(
                ExprKind::Binary {
                    left: Box::new(expr),
                    op,
                    right: Box::new(right),
                },
                span,
            );
        }
        Ok(expr)
    }

    fn comparison(&mut self) -> KuResult<Expr> {
        let mut expr = self.term()?;
        while self.match_any(&[
            TokenKind::Less,
            TokenKind::LessEqual,
            TokenKind::Greater,
            TokenKind::GreaterEqual,
        ]) {
            let op = match self.previous().kind {
                TokenKind::Less => BinaryOp::Less,
                TokenKind::LessEqual => BinaryOp::LessEqual,
                TokenKind::Greater => BinaryOp::Greater,
                _ => BinaryOp::GreaterEqual,
            };
            let right = self.term()?;
            let span = expr.span.merge(right.span);
            expr = Expr::new(
                ExprKind::Binary {
                    left: Box::new(expr),
                    op,
                    right: Box::new(right),
                },
                span,
            );
        }
        Ok(expr)
    }

    fn term(&mut self) -> KuResult<Expr> {
        let mut expr = self.factor()?;
        while self.match_any(&[TokenKind::Minus, TokenKind::Plus]) {
            let op = match self.previous().kind {
                TokenKind::Minus => BinaryOp::Subtract,
                _ => BinaryOp::Add,
            };
            let right = self.factor()?;
            let span = expr.span.merge(right.span);
            expr = Expr::new(
                ExprKind::Binary {
                    left: Box::new(expr),
                    op,
                    right: Box::new(right),
                },
                span,
            );
        }
        Ok(expr)
    }

    fn factor(&mut self) -> KuResult<Expr> {
        let mut expr = self.unary()?;
        while self.match_any(&[TokenKind::Slash, TokenKind::Star, TokenKind::Percent]) {
            let op = match self.previous().kind {
                TokenKind::Slash => BinaryOp::Divide,
                TokenKind::Percent => BinaryOp::Remainder,
                _ => BinaryOp::Multiply,
            };
            let right = self.unary()?;
            let span = expr.span.merge(right.span);
            expr = Expr::new(
                ExprKind::Binary {
                    left: Box::new(expr),
                    op,
                    right: Box::new(right),
                },
                span,
            );
        }
        Ok(expr)
    }

    fn unary(&mut self) -> KuResult<Expr> {
        if self.match_kind(&TokenKind::Await) {
            let await_span = self.previous().span;
            self.enter_parse_depth()?;
            let value = self.unary();
            self.leave_parse_depth();
            let value = value?;
            return Ok(attach_await(value, await_span.start));
        }
        if self.match_any(&[TokenKind::Bang, TokenKind::Minus]) {
            let op_token = self.previous().clone();
            let op = match op_token.kind {
                TokenKind::Bang => UnaryOp::Not,
                _ => UnaryOp::Negate,
            };
            self.enter_parse_depth()?;
            let expr = self.unary();
            self.leave_parse_depth();
            let expr = expr?;
            let span = op_token.span.merge(expr.span);
            return Ok(Expr::new(
                ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                span,
            ));
        }
        self.call()
    }

    fn call(&mut self) -> KuResult<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.match_kind(&TokenKind::LParen) {
                let mut args = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    loop {
                        args.push(self.expression()?);
                        if !self.match_kind(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                let paren = self.consume(&TokenKind::RParen, "expected ')' after arguments")?;
                let span = expr.span.merge(paren.span);
                expr = Expr::new(
                    ExprKind::Call {
                        callee: Box::new(expr),
                        args,
                    },
                    span,
                );
            } else if self.match_kind(&TokenKind::LBracket) {
                let index = self.expression()?;
                let bracket = self.consume(&TokenKind::RBracket, "expected ']' after index")?;
                let span = expr.span.merge(bracket.span);
                expr = Expr::new(
                    ExprKind::Index {
                        target: Box::new(expr),
                        index: Box::new(index),
                    },
                    span,
                );
            } else if self.match_kind(&TokenKind::Dot) {
                let (name, name_span) = self.consume_ident("expected field name after '.'")?;
                let span = expr.span.merge(name_span);
                expr = Expr::new(
                    ExprKind::Field {
                        target: Box::new(expr),
                        name,
                    },
                    span,
                );
            } else if self.match_kind(&TokenKind::QuestionDot) {
                let (name, name_span) = self.consume_ident("expected field name after '?.'")?;
                let span = expr.span.merge(name_span);
                expr = Expr::new(
                    ExprKind::OptionalField {
                        target: Box::new(expr),
                        name,
                    },
                    span,
                );
            } else if self.match_kind(&TokenKind::Question) {
                let span = expr.span.merge(self.previous().span);
                expr = Expr::new(
                    ExprKind::TryUnwrap {
                        expr: Box::new(expr),
                    },
                    span,
                );
            } else if self.check(&TokenKind::LBrace)
                && self.is_struct_literal_after_lbrace()
                && matches!(expr.kind, ExprKind::Field { target: _, name: _ })
            {
                let name = dotted_expr_name(&expr).ok_or_else(|| {
                    KuError::parse("expected struct name before struct literal", expr.span)
                })?;
                let start = expr.span;
                self.advance();
                expr = self.finish_struct_literal(name, start)?;
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn primary(&mut self) -> KuResult<Expr> {
        if self.is_arrow_function_start() {
            return self.arrow_function();
        }
        if self.match_kind(&TokenKind::Match) {
            return self.match_expression(self.previous().span);
        }
        if self.match_kind(&TokenKind::Switch) {
            return Err(KuError::parse(
                "switch is not supported; use match",
                self.previous().span,
            ));
        }
        let token = self.advance().clone();
        let span = token.span;
        let kind = match token.kind {
            TokenKind::Int(value) => ExprKind::Literal(Literal::Int(value)),
            TokenKind::Float(value) => ExprKind::Literal(Literal::Float(value)),
            TokenKind::String(value) => ExprKind::Literal(Literal::String(value)),
            TokenKind::TemplateString(value) => ExprKind::Literal(Literal::TemplateString(value)),
            TokenKind::True => ExprKind::Literal(Literal::Bool(true)),
            TokenKind::False => ExprKind::Literal(Literal::Bool(false)),
            TokenKind::Null => ExprKind::Literal(Literal::Null),
            TokenKind::Ident(name) => {
                if self.match_kind(&TokenKind::Colon) {
                    let ty = self.type_name()?;
                    self.consume(
                        &TokenKind::Arrow,
                        "expected '=>' after typed arrow function parameter",
                    )?;
                    let (body, body_span) = self.arrow_body()?;
                    return Ok(Expr::new(
                        ExprKind::Function {
                            params: vec![FunctionParam {
                                name,
                                ty: Some(ty),
                                span,
                            }],
                            return_type: None,
                            body,
                        },
                        Span::new(span.start, body_span.end),
                    ));
                }
                if self.match_kind(&TokenKind::Arrow) {
                    let (body, body_span) = self.arrow_body()?;
                    return Ok(Expr::new(
                        ExprKind::Function {
                            params: vec![FunctionParam {
                                name,
                                ty: None,
                                span,
                            }],
                            return_type: None,
                            body,
                        },
                        Span::new(span.start, body_span.end),
                    ));
                }
                if self.check(&TokenKind::LBrace) && self.is_struct_literal_after_lbrace() {
                    self.advance();
                    return self.finish_struct_literal(name, span);
                }
                ExprKind::Variable(name)
            }
            TokenKind::LBracket => return self.array_literal(span),
            TokenKind::LBrace => return self.object_literal(span),
            TokenKind::LParen => {
                let expr = self.expression()?;
                self.consume(&TokenKind::RParen, "expected ')' after expression")?;
                return Ok(expr);
            }
            _ => return Err(KuError::parse("expected expression", span)),
        };
        Ok(Expr::new(kind, span))
    }

    fn array_literal(&mut self, start_span: Span) -> KuResult<Expr> {
        let mut values = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            loop {
                values.push(self.expression()?);
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let end = self
            .consume(&TokenKind::RBracket, "expected ']' after array literal")?
            .span;
        Ok(Expr::new(
            ExprKind::Array(values),
            Span::new(start_span.start, end.end),
        ))
    }

    fn object_literal(&mut self, start_span: Span) -> KuResult<Expr> {
        let mut fields = Vec::new();
        if !self.check(&TokenKind::RBrace) {
            loop {
                let token = self.advance().clone();
                let field_name = match token.kind {
                    TokenKind::Ident(name) | TokenKind::String(name) => name,
                    _ => return Err(KuError::parse("expected object field name", token.span)),
                };
                self.consume(&TokenKind::Colon, "expected ':' after object field")?;
                let value = self.expression()?;
                fields.push((field_name, value));
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let end = self
            .consume(&TokenKind::RBrace, "expected '}' after object literal")?
            .span;
        Ok(Expr::new(
            ExprKind::ObjectLiteral { fields },
            Span::new(start_span.start, end.end),
        ))
    }

    fn finish_struct_literal(&mut self, name: String, start_span: Span) -> KuResult<Expr> {
        let mut fields = Vec::new();
        if !self.check(&TokenKind::RBrace) {
            loop {
                let (field_name, _) = self.consume_ident("expected struct literal field name")?;
                self.consume(&TokenKind::Colon, "expected ':' after struct literal field")?;
                let value = self.expression()?;
                fields.push((field_name, value));
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let end = self
            .consume(&TokenKind::RBrace, "expected '}' after struct literal")?
            .span;
        Ok(Expr::new(
            ExprKind::StructLiteral { name, fields },
            Span::new(start_span.start, end.end),
        ))
    }

    fn arrow_function(&mut self) -> KuResult<Expr> {
        let start = self.consume(&TokenKind::LParen, "expected '('")?.span.start;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                let (name, span) = self.consume_ident("expected arrow function parameter")?;
                let ty = if self.match_kind(&TokenKind::Colon) {
                    Some(self.type_name()?)
                } else {
                    None
                };
                params.push(FunctionParam { name, ty, span });
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.consume(
            &TokenKind::RParen,
            "expected ')' after arrow function parameters",
        )?;
        let return_type = if self.match_kind(&TokenKind::Colon) {
            Some(self.type_name()?)
        } else {
            None
        };
        self.consume(
            &TokenKind::Arrow,
            "expected '=>' after arrow function parameters",
        )?;
        let (body, body_span) = self.arrow_body()?;
        Ok(Expr::new(
            ExprKind::Function {
                params,
                return_type,
                body,
            },
            Span::new(start, body_span.end),
        ))
    }

    fn arrow_body(&mut self) -> KuResult<(Vec<Stmt>, Span)> {
        if self.check(&TokenKind::LBrace) {
            return self.block();
        }
        let value = self.expression()?;
        let span = value.span;
        Ok((
            vec![Stmt::Return {
                value: Some(value),
                span,
            }],
            span,
        ))
    }

    fn match_expression(&mut self, start_span: Span) -> KuResult<Expr> {
        let value = self.expression()?;
        self.consume(&TokenKind::LBrace, "expected '{' after match value")?;
        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            let arm_start = self.peek().span;
            let pattern = self.match_pattern()?;
            let guard = if self.match_kind(&TokenKind::If) {
                Some(self.expression()?)
            } else {
                None
            };
            self.consume(&TokenKind::Arrow, "expected '=>' after match pattern")?;
            let value = self.expression()?;
            let span = Span::new(arm_start.start, value.span.end);
            arms.push(MatchArm {
                pattern,
                guard,
                value,
                span,
            });
            self.match_kind(&TokenKind::Comma);
            self.optional_semicolon();
        }
        let end = self
            .consume(&TokenKind::RBrace, "expected '}' after match arms")?
            .span;
        Ok(Expr::new(
            ExprKind::Match {
                value: Box::new(value),
                arms,
            },
            Span::new(start_span.start, end.end),
        ))
    }

    fn match_pattern(&mut self) -> KuResult<MatchPattern> {
        let token = self.advance().clone();
        match token.kind {
            TokenKind::Ident(name) if name == "_" => Ok(MatchPattern::Wildcard),
            TokenKind::Int(value) => Ok(MatchPattern::Literal(Literal::Int(value))),
            TokenKind::Float(value) => Ok(MatchPattern::Literal(Literal::Float(value))),
            TokenKind::String(value) => Ok(MatchPattern::Literal(Literal::String(value))),
            TokenKind::True => Ok(MatchPattern::Literal(Literal::Bool(true))),
            TokenKind::False => Ok(MatchPattern::Literal(Literal::Bool(false))),
            TokenKind::Null => Ok(MatchPattern::Literal(Literal::Null)),
            TokenKind::Ident(enum_name) => {
                if !self.match_kind(&TokenKind::Dot) {
                    return Ok(MatchPattern::Binding(enum_name));
                }
                let (mut variant, _) =
                    self.consume_ident("expected enum variant in match pattern")?;
                let enum_name = if self.match_kind(&TokenKind::Dot) {
                    let namespace = enum_name;
                    let enum_type = variant;
                    let (actual_variant, _) =
                        self.consume_ident("expected enum variant in match pattern")?;
                    variant = actual_variant;
                    format!("{namespace}.{enum_type}")
                } else {
                    enum_name
                };
                let mut fields = Vec::new();
                if self.match_kind(&TokenKind::LParen) {
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            fields.push(self.match_pattern()?);
                            if !self.match_kind(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.consume(&TokenKind::RParen, "expected ')' after payload patterns")?;
                }
                Ok(MatchPattern::EnumVariant {
                    enum_name,
                    variant,
                    fields,
                })
            }
            _ => Err(KuError::parse("expected match pattern", token.span)),
        }
    }

    fn is_arrow_function_start(&self) -> bool {
        if !self.check(&TokenKind::LParen) {
            return false;
        }
        let mut index = self.current + 1;
        if matches!(
            self.tokens.get(index).map(|token| &token.kind),
            Some(TokenKind::RParen)
        ) {
            index += 1;
        } else {
            loop {
                if !matches!(
                    self.tokens.get(index).map(|token| &token.kind),
                    Some(TokenKind::Ident(_))
                ) {
                    return false;
                }
                index += 1;
                if matches!(
                    self.tokens.get(index).map(|token| &token.kind),
                    Some(TokenKind::Colon)
                ) {
                    index += 1;
                    if !scan_arrow_type(&self.tokens, &mut index, true) {
                        return false;
                    }
                }
                match self.tokens.get(index).map(|token| &token.kind) {
                    Some(TokenKind::Comma) => index += 1,
                    Some(TokenKind::RParen) => {
                        index += 1;
                        break;
                    }
                    _ => return false,
                }
            }
        }
        if matches!(
            self.tokens.get(index).map(|token| &token.kind),
            Some(TokenKind::Colon)
        ) {
            index += 1;
            if !scan_arrow_type(&self.tokens, &mut index, false) {
                return false;
            }
        }
        matches!(
            self.tokens.get(index).map(|token| &token.kind),
            Some(TokenKind::Arrow)
        )
    }

    fn is_struct_literal_after_lbrace(&self) -> bool {
        match self.tokens.get(self.current + 1).map(|token| &token.kind) {
            Some(TokenKind::Ident(_)) => matches!(
                self.tokens.get(self.current + 2).map(|token| &token.kind),
                Some(TokenKind::Colon)
            ),
            _ => false,
        }
    }

    fn optional_semicolon(&mut self) {
        self.match_kind(&TokenKind::Semicolon);
    }

    fn consume(&mut self, kind: &TokenKind, message: &str) -> KuResult<&Token> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            Err(KuError::parse(message, self.peek().span))
        }
    }

    fn consume_ident(&mut self, message: &str) -> KuResult<(String, Span)> {
        let token = self.advance().clone();
        match token.kind {
            TokenKind::Ident(name) => Ok((name, token.span)),
            _ => Err(KuError::parse(message, token.span)),
        }
    }

    fn match_ident_text(&mut self, text: &str) -> bool {
        match &self.peek().kind {
            TokenKind::Ident(value) if value == text => {
                self.advance();
                true
            }
            _ => false,
        }
    }

    fn consume_string(&mut self, message: &str) -> KuResult<(String, Span)> {
        let token = self.advance().clone();
        match token.kind {
            TokenKind::String(value) => Ok((value, token.span)),
            _ => Err(KuError::parse(message, token.span)),
        }
    }

    fn match_any(&mut self, kinds: &[TokenKind]) -> bool {
        for kind in kinds {
            if self.check(kind) {
                self.advance();
                return true;
            }
        }
        false
    }

    fn match_kind(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn check(&self, kind: &TokenKind) -> bool {
        token_kind_eq(&self.peek().kind, kind)
    }

    fn advance(&mut self) -> &Token {
        if !self.check(&TokenKind::Eof) {
            self.current += 1;
        }
        self.previous()
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.current]
    }

    fn previous(&self) -> &Token {
        &self.tokens[self.current - 1]
    }

    fn peek_next_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.current + 1).map(|token| &token.kind)
    }

    fn enter_parse_depth(&mut self) -> KuResult<()> {
        self.parse_depth += 1;
        if self.parse_depth > MAX_PARSE_DEPTH {
            Err(KuError::parse(
                "maximum parse depth exceeded; expression is too deeply nested",
                self.peek().span,
            ))
        } else {
            Ok(())
        }
    }

    fn leave_parse_depth(&mut self) {
        self.parse_depth = self.parse_depth.saturating_sub(1);
    }
}

fn token_kind_eq(left: &TokenKind, right: &TokenKind) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

fn is_exported_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn is_valid_namespace(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

fn attach_await(value: Expr, await_start: Position) -> Expr {
    match value {
        Expr {
            kind: ExprKind::TryUnwrap { expr },
            span: try_span,
        } => Expr::new(
            ExprKind::TryUnwrap {
                expr: Box::new(attach_await(*expr, await_start)),
            },
            try_span,
        ),
        value => {
            let span = Span::new(await_start, value.span.end);
            Expr::new(ExprKind::Await(Box::new(value)), span)
        }
    }
}

fn scan_arrow_type(tokens: &[Token], index: &mut usize, parameter: bool) -> bool {
    let mut bracket_depth = 0usize;
    let mut consumed_atom = false;
    while let Some(token) = tokens.get(*index) {
        match &token.kind {
            TokenKind::Ident(_) | TokenKind::Null => {
                consumed_atom = true;
                *index += 1;
            }
            TokenKind::LBracket => {
                bracket_depth += 1;
                *index += 1;
            }
            TokenKind::RBracket if bracket_depth > 0 => {
                bracket_depth -= 1;
                *index += 1;
            }
            TokenKind::Dot | TokenKind::Bang | TokenKind::Pipe if consumed_atom => {
                *index += 1;
            }
            TokenKind::Comma | TokenKind::RParen
                if parameter && bracket_depth == 0 && consumed_atom =>
            {
                return true;
            }
            TokenKind::Arrow if !parameter && bracket_depth == 0 && consumed_atom => return true,
            _ => return false,
        }
    }
    false
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

fn assign_target_from_expr(expr: Expr) -> KuResult<AssignTarget> {
    match expr.kind {
        ExprKind::Variable(name) => Ok(AssignTarget::Variable(name)),
        ExprKind::Index { target, index } => Ok(AssignTarget::Index {
            target: *target,
            index: *index,
        }),
        ExprKind::Field { target, name } => Ok(AssignTarget::Field {
            target: *target,
            name,
        }),
        _ => Err(KuError::parse("invalid assignment target", expr.span)),
    }
}

fn dotted_expr_name(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name.clone()),
        ExprKind::Field { target, name } => {
            let target = dotted_expr_name(target)?;
            Some(format!("{target}.{name}"))
        }
        _ => None,
    }
}

fn default_expr(ty: &TypeName, span: Span) -> Expr {
    let kind = match ty {
        TypeName::Int => ExprKind::Literal(Literal::Int(0)),
        TypeName::Float => ExprKind::Literal(Literal::Float(0.0)),
        TypeName::Bool => ExprKind::Literal(Literal::Bool(false)),
        TypeName::String => ExprKind::Literal(Literal::String(String::new())),
        TypeName::Array(_) => ExprKind::Array(Vec::new()),
        TypeName::Result(_) | TypeName::Union(_) | TypeName::Null | TypeName::Custom(_) => {
            ExprKind::Literal(Literal::Null)
        }
    };
    Expr::new(kind, span)
}

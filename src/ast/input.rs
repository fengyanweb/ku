//! The static description shared by the checker and both execution backends.

use crate::error::{KuError, KuResult};
use crate::span::Span;

use super::{Expr, ExprKind, Literal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Text,
    Int,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// Text before each field, followed by the final suffix.
    pub(crate) segments: Vec<String>,
    pub(crate) fields: Vec<Field>,
}

pub(crate) const NAME: &str = "input";
const MAX_FIELDS: usize = 256;

pub(crate) fn direct_call(expr: &Expr) -> Option<&[Expr]> {
    let ExprKind::Call { callee, args } = &expr.kind else {
        return None;
    };
    matches!(&callee.kind, ExprKind::Variable(name) if name == NAME).then_some(args)
}

pub(crate) fn destructure_call(values: &[Expr]) -> Option<&Expr> {
    let [value] = values else { return None };
    let ExprKind::TryUnwrap { expr } = &value.kind else {
        return None;
    };
    direct_call(expr).map(|_| &**expr)
}

pub(crate) fn prepare_argument(argument: &mut Expr) {
    if let ExprKind::Literal(Literal::TemplateString(raw)) = &argument.kind {
        // An invalid candidate gets its diagnostic when an unshadowed builtin
        // is checked. A user function named input still uses ordinary interpolation.
        if let Ok(Some(plan)) = parse_template(raw, argument.span) {
            argument.kind = ExprKind::Literal(Literal::InputTemplate(Box::new(plan)));
        }
    }
}

pub(crate) fn template(argument: &Expr) -> KuResult<Option<&Template>> {
    match &argument.kind {
        ExprKind::Literal(Literal::InputTemplate(plan)) => Ok(Some(plan)),
        ExprKind::Literal(Literal::TemplateString(raw)) => {
            // Valid fill templates have already been prepared by the parser.
            let _ = parse_template(raw, argument.span)?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn parse_template(raw: &str, span: Span) -> KuResult<Option<Template>> {
    if !raw.contains("${") {
        return Ok(None);
    }

    let mut chars = raw.char_indices().peekable();
    let mut segments = Vec::new();
    let mut fields = Vec::new();
    let mut last_field_end = 0;
    let mut has_interpolation = false;
    while let Some((start, ch)) = chars.next() {
        if ch == '\\' {
            chars.next();
            continue;
        }
        let is_slot = ch == '$' && chars.peek().is_some_and(|(_, next)| *next == '{');
        let content_start = if is_slot {
            chars.next();
            start + 2
        } else if ch == '{' {
            has_interpolation = true;
            start + 1
        } else {
            continue;
        };

        let end = loop {
            match chars.next() {
                Some((end, '}')) => break end,
                Some((_, '\\')) => {
                    chars.next();
                }
                Some(_) => {}
                None if is_slot => {
                    return Err(KuError::parse("unterminated input placeholder", span));
                }
                None => break raw.len(),
            }
        };
        if !is_slot {
            continue;
        }

        let field = match &raw[content_start..end] {
            "" => Field::Text,
            ":int" => Field::Int,
            content if content.trim().is_empty() || content.trim_start().starts_with(':') => {
                return Err(KuError::parse(
                    "invalid input placeholder; use '${}' for text or '${:int}' for an integer",
                    span,
                ));
            }
            _ => {
                // Ku already allows a literal '$' immediately before a
                // normal `{expression}`. Preserve it for no-hole prompts.
                has_interpolation = true;
                continue;
            }
        };
        if fields.len() == MAX_FIELDS {
            return Err(KuError::parse(
                format!("input template exceeds {MAX_FIELDS} placeholders"),
                span,
            ));
        }
        segments.push(literal_text(&raw[last_field_end..start]));
        fields.push(field);
        last_field_end = end + 1;
    }

    if fields.is_empty() {
        return Ok(None);
    }
    if has_interpolation {
        return Err(KuError::parse(
            "input fill templates cannot mix placeholders with expression interpolation",
            span,
        ));
    }
    segments.push(literal_text(&raw[last_field_end..]));
    Ok(Some(Template { segments, fields }))
}

fn literal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        // The lexer has already decoded the other template escapes. Escaped
        // braces are retained there so the template scanner can skip them.
        if ch == '\\' && matches!(chars.peek(), Some('{' | '}')) {
            out.extend(chars.next());
        } else {
            out.push(ch);
        }
    }
    out
}

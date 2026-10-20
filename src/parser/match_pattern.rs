use super::*;

const SCRATCH_BYTES: usize = 2 * 1024 * 1024;

struct PatternFrame {
    enum_name: String,
    variant: String,
    fields: Vec<MatchPattern>,
}

impl PatternFrame {
    fn finish(self) -> MatchPattern {
        MatchPattern::EnumVariant {
            enum_name: self.enum_name,
            variant: self.variant,
            fields: self.fields,
        }
    }
}

// The original grammar has no semantic pattern-depth limit. Bound only the
// actual temporary frame storage; completed nodes retain their AST ownership.
fn reserve_frame(frames: &mut Vec<PatternFrame>, bytes: usize, span: Span) -> KuResult<()> {
    let limit = bytes / std::mem::size_of::<PatternFrame>();
    let exhausted = || KuError::parse("match pattern construction scratch budget exhausted", span);
    let required = frames.len().checked_add(1).ok_or_else(exhausted)?;
    if required > limit || frames.capacity() > limit {
        return Err(exhausted());
    }
    if required <= frames.capacity() {
        return Ok(());
    }
    // Include the old and replacement backing at the moment they coexist.
    let available = limit - frames.capacity();
    if required > available {
        return Err(exhausted());
    }
    let target = frames
        .capacity()
        .saturating_mul(2)
        .max(required)
        .min(available);
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(target)
        .map_err(|_| KuError::parse("match pattern construction scratch out of memory", span))?;
    if replacement.capacity() > available {
        return Err(exhausted());
    }
    replacement.append(frames);
    *frames = replacement;
    Ok(())
}

pub(super) fn parse(parser: &mut Parser) -> KuResult<MatchPattern> {
    let mut frames = Vec::new();
    'next_pattern: loop {
        let token = parser.advance().clone();
        let mut completed = match token.kind {
            TokenKind::Ident(name) if name == "_" => MatchPattern::Wildcard,
            TokenKind::Int(value) => MatchPattern::Literal(Literal::Int(value)),
            TokenKind::Float(value) => MatchPattern::Literal(Literal::Float(value)),
            TokenKind::String(value) => MatchPattern::Literal(Literal::String(value)),
            TokenKind::True => MatchPattern::Literal(Literal::Bool(true)),
            TokenKind::False => MatchPattern::Literal(Literal::Bool(false)),
            TokenKind::Null => MatchPattern::Literal(Literal::Null),
            TokenKind::Ident(enum_name) => {
                if !parser.match_kind(&TokenKind::Dot) {
                    MatchPattern::Binding(enum_name)
                } else {
                    let (mut variant, _) =
                        parser.consume_ident("expected enum variant in match pattern")?;
                    let enum_name = if parser.match_kind(&TokenKind::Dot) {
                        let (actual, _) =
                            parser.consume_ident("expected enum variant in match pattern")?;
                        let qualified = format!("{enum_name}.{variant}");
                        variant = actual;
                        qualified
                    } else {
                        enum_name
                    };
                    let frame = PatternFrame {
                        enum_name,
                        variant,
                        fields: Vec::new(),
                    };
                    if parser.match_kind(&TokenKind::LParen) {
                        if !parser.check(&TokenKind::RParen) {
                            reserve_frame(&mut frames, SCRATCH_BYTES, token.span)?;
                            frames.push(frame);
                            continue 'next_pattern;
                        }
                        parser
                            .consume(&TokenKind::RParen, "expected ')' after payload patterns")?;
                    }
                    frame.finish()
                }
            }
            _ => return Err(KuError::parse("expected match pattern", token.span)),
        };
        loop {
            let Some(parent) = frames.last_mut() else {
                return Ok(completed);
            };
            parent.fields.push(completed);
            if parser.match_kind(&TokenKind::Comma) {
                continue 'next_pattern;
            }
            parser.consume(&TokenKind::RParen, "expected ')' after payload patterns")?;
            completed = frames
                .pop()
                .expect("the completed parent is present")
                .finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_budget_charges_growth_peak_and_preserves_frames_on_rejection() {
        let mut frames = Vec::new();
        let span = Span::default();
        let bytes = std::mem::size_of::<PatternFrame>();
        reserve_frame(&mut frames, bytes, span).unwrap();
        frames.push(PatternFrame {
            enum_name: "Kept".into(),
            variant: "Wrap".into(),
            fields: Vec::new(),
        });
        let before = (frames.as_ptr(), frames.capacity());
        assert!(reserve_frame(&mut frames, 3 * bytes - 1, span).is_err());
        assert_eq!((frames.as_ptr(), frames.capacity()), before);
        assert_eq!(frames[0].enum_name, "Kept");
        reserve_frame(&mut frames, 3 * bytes, span).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames.capacity(), 2);
    }
}

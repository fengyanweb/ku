use ku::lexer::Lexer;
use ku::token::TokenKind;

#[test]
fn borrow_lexer_ampersand_uses_longest_match_with_exact_spans() {
    let source = "& && &&& &&&& a & b";
    let tokens = Lexer::new(source).lex().unwrap();
    let expected = [
        (TokenKind::Ampersand, 0, 1),
        (TokenKind::AndAnd, 2, 4),
        (TokenKind::AndAnd, 5, 7),
        (TokenKind::Ampersand, 7, 8),
        (TokenKind::AndAnd, 9, 11),
        (TokenKind::AndAnd, 11, 13),
        (TokenKind::Ident("a".into()), 14, 15),
        (TokenKind::Ampersand, 16, 17),
        (TokenKind::Ident("b".into()), 18, 19),
        (TokenKind::Eof, 19, 19),
    ];
    assert_eq!(tokens.len(), expected.len());
    for (token, (kind, start, end)) in tokens.iter().zip(expected) {
        assert_eq!(token.kind, kind);
        assert_eq!(token.span.start.offset, start);
        assert_eq!(token.span.end.offset, end);
        assert_eq!(token.span.start.column, start + 1);
        assert_eq!(token.span.end.column, end + 1);
    }
}

#[test]
fn borrow_lexer_ampersand_does_not_change_literals_comments_or_identifiers() {
    let tokens = Lexer::new("\"&\" '&' `value & text` // &\n/* & */ view page.view ui.view()")
        .lex()
        .unwrap();
    let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();
    assert_eq!(
        kinds,
        vec![
            TokenKind::String("&".into()),
            TokenKind::String("&".into()),
            TokenKind::TemplateString("value & text".into()),
            TokenKind::Ident("view".into()),
            TokenKind::Ident("page".into()),
            TokenKind::Dot,
            TokenKind::Ident("view".into()),
            TokenKind::Ident("ui".into()),
            TokenKind::Dot,
            TokenKind::Ident("view".into()),
            TokenKind::LParen,
            TokenKind::RParen,
            TokenKind::Eof,
        ]
    );
}

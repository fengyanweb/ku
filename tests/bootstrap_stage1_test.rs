//! Self-hosted lexical grammar gate, not a parser/compiler bootstrap.
//!
//! The Rust lexer is the oracle for token kinds, decoded text and full spans.
//! Decimal floats are normalized to their original spelling on both sides:
//! the Ku token is lossless and intentionally defers f64 rounding to a parser.
//! No compiler lexer builtin is called by the Ku scanner under test.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use ku::error::KuError;
use ku::lexer::Lexer;
use ku::token::{Token, TokenKind};

const BOOTSTRAP_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(8 * 1024 * 1024, 12 * 1024 * 1024);
const MAX_CHARACTERS: usize = 32_768;
const MAX_TOKENS: usize = 4_096;
const MAX_STRING_CHARACTERS: usize = 4_096;

fn escape_canonical(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('|', "\\p")
}

fn normalized_token(source: &str, token: &Token) -> (String, String, i64) {
    let raw = &source[token.span.start.offset..token.span.end.offset];
    match &token.kind {
        TokenKind::Ident(value) => ("Ident".into(), value.clone(), 0),
        TokenKind::Int(value) => ("Int".into(), raw.into(), *value),
        TokenKind::Float(_) => ("Float".into(), raw.into(), 0),
        TokenKind::String(value) => ("String".into(), value.clone(), 0),
        TokenKind::TemplateString(value) => ("TemplateString".into(), value.clone(), 0),
        kind => (format!("{kind:?}"), raw.into(), 0),
    }
}

fn canonical_tokens(source: &str, tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(|token| {
            let (kind, lexeme, int_value) = normalized_token(source, token);
            let start = token.span.start;
            let end = token.span.end;
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}",
                kind,
                escape_canonical(&lexeme),
                int_value,
                start.line,
                start.column,
                start.offset,
                end.line,
                end.column,
                end.offset
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn canonical_error(error: &KuError) -> String {
    let code = match error.message.as_str() {
        "invalid int literal" => "integer_overflow",
        "unterminated block comment" => "unterminated_block_comment",
        "unterminated string" => "unterminated_string",
        "unterminated string escape" => "unterminated_escape",
        "unterminated template string" => "unterminated_template",
        "unterminated template string escape" => "unterminated_template_escape",
        "expected third '.' for '...'" => "invalid_ellipsis",
        message if message.starts_with("unknown string escape '") => "unknown_escape",
        message if message.starts_with("unexpected character '") => "invalid_character",
        other => panic!("unmapped Rust lexical diagnostic: {other}"),
    };
    let position = error.span.start;
    format!(
        "ERR|bootstrap.lexer|{code}|{}",
        escape_canonical(&format!(
            "{}:{}@{}: {}",
            position.line, position.column, position.offset, error.message
        ))
    )
}

fn oracle(cases: &[(String, String)]) -> String {
    let mut output = String::new();
    for (label, source) in cases {
        output.push_str(&format!("CASE|{label}\n"));
        match Lexer::new(source).lex() {
            Ok(tokens) => output.push_str(&canonical_tokens(source, &tokens)),
            Err(error) => output.push_str(&canonical_error(&error)),
        }
        output.push('\n');
    }
    output
}

fn demo_cases() -> Vec<(String, String)> {
    [
        (
            "function",
            "fn Add(a: int, b: int): int { return a + b }\r\n",
        ),
        ("unicode-span", "\"中😀\" next"),
        ("float", "9223372036854775808.125"),
        ("single-string", "'it\\'s'"),
        ("unknown-escape", "\"oops\\q\""),
    ]
    .into_iter()
    .map(|(label, source)| (label.into(), source.into()))
    .collect()
}

fn grammar_cases() -> Vec<(String, String)> {
    let mut cases: Vec<_> = [
        ("empty", ""),
        ("whitespace", " \t\r\n\u{feff} "),
        (
            "keywords",
            "async await fn struct enum module import from let mut if else while for in break continue match switch try catch finally fail panic return print true false null",
        ),
        ("identifiers", "abc ABC_1 _ _0 truex let_ Fn println"),
        (
            "punctuation",
            "+ ++ += - -- -= * *= / /= % %= ! != ? ?. = == < <= > >= & && || | . ... ( ) { } [ ] , : ;",
        ),
        ("arrows", "=> ->"),
        ("adjacent-punctuation", "a+++b---c?...=>d||e&&f"),
        ("max-int", "9223372036854775807"),
        ("leading-zero-int", "00000000000000000000000042"),
        ("overflow", "9223372036854775808"),
        ("negative-overflow-token", "-9223372036854775808"),
        ("decimals", "0.0 0001.2300 9223372036854775808.0"),
        ("decimal-boundaries", "1. .5 1e3 1.2e-3 1.2.3 1...2 ...."),
        ("double-string", "\"中😀\\n\\r\\t\\\"\\\\\" tail"),
        ("single-string", "'it\\'s' '\\n\\r\\t\\\\' tail"),
        ("double-cross-quote-escape", "\"\\'\""),
        ("single-cross-quote-escape", "'\\\"'"),
        (
            "template",
            "`中😀 {user.name} \\{literal\\} \\q \\` \\\\ \\n \\r \\t` tail",
        ),
        ("template-unicode-unknown-escape", "`\\中 \\😀 \\e\u{301}` next"),
        ("template-newline-escape", "`\\\n` next"),
        ("multiline-string", "\"中\r\n😀\ne\u{301}\" next"),
        ("bom-between-tokens", "\u{feff}a\u{feff}b"),
        ("bom-in-string", "\"\u{feff}\" next"),
        ("nul-in-string", "\"a\0b\" next"),
        ("nul-outside-string", "a\0b"),
        ("unicode-identifier-rejected", "中"),
        ("four-byte-character-rejected", "😀"),
        ("line-comment-cr-only", "first // 中\rnot_a_token"),
        ("line-comment-cr-then-lf", "// 中\rhidden\nvisible"),
        ("line-comment-unicode", "// 中😀\r\nnext"),
        ("block-comment-inline", "let /* same line */ value"),
        ("block-comment-lines", "one/*中\r\n二😀\n*/two"),
        ("block-comment-adjacent", "left/*x*/right"),
        ("block-comment-non-nested", "left/* outer /* inner */right"),
        ("block-comment-following-close", "/* outer /* inner */ close */"),
        ("block-comment-unterminated", "before/*oops\r\n中"),
        ("empty-block-comment", "/**/"),
        ("unterminated-double", "\"oops"),
        ("unterminated-single", "'oops"),
        ("unterminated-template", "`oops"),
        ("unterminated-double-escape", "\"oops\\"),
        ("unterminated-single-escape", "'oops\\"),
        ("unterminated-template-escape", "`oops\\"),
        ("unknown-double-escape", "\"oops\\q\""),
        ("unknown-single-escape", "'oops\\q'"),
        ("unknown-unicode-escape", "\"中\\😀\""),
        ("unknown-newline-escape", "\"中\\\n\""),
        ("invalid-ellipsis", ".."),
        ("invalid-ellipsis-after-prefix", "x ....."),
        ("ampersand", "&"),
        ("ampersand-longest-match", "&& &&& &&&& a & b"),
        ("borrow-parameter", "fn f(&x: T) {} fn f(&x) {}"),
        ("borrow-function-type", "fn(&T): R"),
        ("ampersand-string", "\"&\" '&' `value & text`"),
        ("ampersand-comments", "// &\n/* & */ view page.view ui.view()"),
        ("invalid-character", "@"),
        ("non-whitespace-unicode", "\u{a0}"),
    ]
    .into_iter()
    .map(|(label, source)| (label.into(), source.into()))
    .collect();
    cases.push(("large-float".into(), format!("{}.0", "9".repeat(400))));
    cases.push(("tiny-float".into(), format!("0.{}1", "0".repeat(400))));

    // Fixed-seed short-input differential testing. Cases are bounded by
    // construction; the subprocess also has an output cap and a deadline.
    let alphabet: Vec<_> = "azAZ_09.+-*/%=!?<>&|(){}[],:; \t\r\n\"'`\\@中😀\u{301}\u{feff}\0"
        .chars()
        .collect();
    let fragments = [
        "fn ",
        "name_1 ",
        "9223372036854775807 ",
        "0001.2300 ",
        "\"中😀\" ",
        "'it\\'s' ",
        "`a {b} \\q` ",
        "/*中\n*/",
        "//x\rhidden\n",
        "=> ",
        "... ",
        "true ",
        "(){}[]; ",
    ];
    let mut seed = 0x4b55_4c45_5845_5201_u64;
    for case in 0..256 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let length = 1 + ((seed >> 32) as usize % 64);
        let mut source = String::new();
        for _ in 0..length {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            if case % 2 == 0 {
                source.push(alphabet[(seed >> 32) as usize % alphabet.len()]);
            } else {
                source.push_str(fragments[(seed >> 32) as usize % fragments.len()]);
            }
        }
        cases.push((format!("seeded-{case}"), source));
    }
    cases
}

fn collect_ku_sources(directory: &Path, paths: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read corpus {}: {error}", directory.display()))
        .map(|entry| entry.expect("read corpus entry"))
        .collect();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let kind = entry.file_type().expect("read corpus entry type");
        let path = entry.path();
        assert!(
            !kind.is_symlink(),
            "corpus must not follow a symlink: {}",
            path.display()
        );
        if kind.is_dir() {
            collect_ku_sources(&path, paths);
        } else if path.extension().is_some_and(|extension| extension == "ku") {
            paths.push(path);
        }
    }
}

fn differential_cases() -> Vec<(String, String)> {
    let mut cases = grammar_cases();
    let mut paths = Vec::new();
    for directory in ["bootstrap", "examples"] {
        collect_ku_sources(&repo_root().join(directory), &mut paths);
    }
    assert!(
        !paths.is_empty(),
        "repository corpus must not be silently skipped"
    );
    let corpus_count = paths.len();
    for path in paths {
        let relative = path
            .strip_prefix(repo_root())
            .expect("corpus stays in repository");
        cases.push((
            format!("corpus/{}", relative.display()).replace('\\', "/"),
            fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read corpus source {}: {error}", path.display())),
        ));
    }
    let mut kinds = BTreeSet::new();
    let mut total_characters = 0;
    let mut total_tokens = 0;
    for (label, source) in &cases {
        let characters = source.chars().count();
        assert!(
            characters <= MAX_CHARACTERS,
            "{label} has {characters} characters, exceeding the explicit lexer cap; do not skip it"
        );
        total_characters += characters;
        if let Ok(tokens) = Lexer::new(source).lex() {
            if label.starts_with("corpus/") {
                eprintln!(
                    "bootstrap corpus {label}: {characters} characters, {} tokens",
                    tokens.len()
                );
            }
            assert!(
                tokens.len() <= MAX_TOKENS,
                "{label} has {} tokens, exceeding the explicit lexer cap; do not skip it",
                tokens.len()
            );
            total_tokens += tokens.len();
            for token in &tokens {
                let (kind, _, _) = normalized_token(source, token);
                kinds.insert(kind);
                if let TokenKind::String(value) | TokenKind::TemplateString(value) = &token.kind {
                    assert!(
                        value.chars().count() <= MAX_STRING_CHARACTERS,
                        "{label} contains a decoded string exceeding the explicit lexer cap"
                    );
                }
            }
        }
    }
    let expected_kinds: BTreeSet<String> = [
        "Async",
        "Await",
        "Fn",
        "Struct",
        "Enum",
        "Module",
        "Import",
        "From",
        "Let",
        "Mut",
        "If",
        "Else",
        "While",
        "For",
        "In",
        "Break",
        "Continue",
        "Match",
        "Switch",
        "Try",
        "Catch",
        "Finally",
        "Fail",
        "Panic",
        "Return",
        "Print",
        "True",
        "False",
        "Null",
        "Ident",
        "Int",
        "Float",
        "String",
        "TemplateString",
        "Plus",
        "PlusPlus",
        "PlusEqual",
        "Minus",
        "MinusMinus",
        "MinusEqual",
        "Star",
        "StarEqual",
        "Slash",
        "SlashEqual",
        "Percent",
        "PercentEqual",
        "Bang",
        "Question",
        "QuestionDot",
        "BangEqual",
        "Equal",
        "Arrow",
        "EqualEqual",
        "Less",
        "LessEqual",
        "Greater",
        "GreaterEqual",
        "Ampersand",
        "AndAnd",
        "OrOr",
        "Pipe",
        "Dot",
        "Ellipsis",
        "LParen",
        "RParen",
        "LBrace",
        "RBrace",
        "LBracket",
        "RBracket",
        "Comma",
        "Colon",
        "Semicolon",
        "Eof",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        kinds, expected_kinds,
        "every current Rust TokenKind needs a parity case"
    );
    eprintln!(
        "bootstrap differential: {} cases ({corpus_count} repository sources), {total_characters} characters, {total_tokens} successful tokens, {} token kinds",
        cases.len(), kinds.len()
    );
    cases
}

fn quote_ku(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\r', "\\r")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    )
}

fn ku_string_expression(text: &str) -> String {
    // Bound each original UTF-8 chunk to 3 KiB, so even octal/hex expansion
    // stays below MSVC's C string-literal limit. Split only at scalar boundaries.
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for ch in text.chars() {
        if chunk.len() + ch.len_utf8() > 3_072 {
            chunks.push(quote_ku(&chunk));
            chunk.clear();
        }
        chunk.push(ch);
    }
    chunks.push(quote_ku(&chunk));
    fn balanced(chunks: &[String]) -> String {
        if chunks.len() == 1 {
            return chunks[0].clone();
        }
        let middle = chunks.len() / 2;
        format!(
            "({} + {})",
            balanced(&chunks[..middle]),
            balanced(&chunks[middle..])
        )
    }
    // Large Unicode boundary inputs need many portable literals. A balanced
    // expression avoids an artificial checker-depth failure and prefix copies
    // in the fixture's input construction.
    balanced(&chunks)
}

fn differential_driver(cases: &[(String, String)]) -> String {
    let mut driver = String::from(
        r#"import { Scan, Canonical, EscapeCanonical } from "./lexer.ku"

fn RunCase(label: str, source: str): null! {
    println("CASE|" + label)
    try {
        tokens = Scan(source)?
        println(Canonical(tokens))
    } catch(err) {
        println("ERR|" + err.domain + "|" + err.code + "|" + EscapeCanonical(err.message))
    }
    return ok(null)
}

fn main(): null! {
"#,
    );
    for (label, source) in cases {
        driver.push_str(&format!(
            "    RunCase({}, {})?\n",
            quote_ku(label),
            ku_string_expression(source)
        ));
    }
    driver.push_str("    return ok(null)\n}\n");
    driver
}

fn boundary_expected() -> String {
    let source = "0 ".repeat(MAX_TOKENS - 1);
    let tokens = Lexer::new(&source).lex().expect("boundary source lexes");
    let canonical_length = canonical_tokens(&source, &tokens).chars().count();
    let mut expected = format!(
        "BOUNDARY|input-at-limit\nOK|1\n\
         BOUNDARY|input-over-limit\nERR|input_too_large\n\
         BOUNDARY|string-at-limit\nOK|2\n\
         BOUNDARY|string-over-limit\nERR|string_too_large\n\
         BOUNDARY|tokens-at-limit\nOK|4096\nCANONICAL|{canonical_length}\n\
         BOUNDARY|tokens-over-limit\nERR|too_many_tokens\n"
    );
    for (label, source, result) in additional_boundaries() {
        expected.push_str(&format!("BOUNDARY|{label}\n{result}\n"));
        if result == "OK|4096" {
            let tokens = Lexer::new(&source)
                .lex()
                .expect("token-boundary source lexes");
            let canonical_length = canonical_tokens(&source, &tokens).chars().count();
            expected.push_str(&format!("CANONICAL|{canonical_length}\n"));
        }
    }
    expected
}

fn additional_boundaries() -> Vec<(&'static str, String, &'static str)> {
    vec![
        (
            "ampersand-tokens-at-limit",
            "& ".repeat(MAX_TOKENS - 1),
            "OK|4096",
        ),
        (
            "ampersand-tokens-over-limit",
            "& ".repeat(MAX_TOKENS),
            "ERR|too_many_tokens",
        ),
        (
            "and-and-tokens-at-limit",
            "&".repeat((MAX_TOKENS - 1) * 2),
            "OK|4096",
        ),
        (
            "and-and-tokens-over-limit",
            "&".repeat((MAX_TOKENS - 1) * 2 + 1),
            "ERR|too_many_tokens",
        ),
        (
            "unicode-comment-at-limit",
            format!("//{}", "😀".repeat(MAX_CHARACTERS - 2)),
            "OK|1",
        ),
        (
            "unicode-comment-over-limit",
            format!("//{}", "😀".repeat(MAX_CHARACTERS - 1)),
            "ERR|input_too_large",
        ),
        (
            "byte-guard-at-limit",
            "😀".repeat(MAX_CHARACTERS),
            // This input fits the character/byte bounds but is lexically
            // invalid; it must reach the character diagnostic, not the cap.
            "ERR|invalid_character",
        ),
        (
            "byte-guard-over-limit",
            "😀".repeat(MAX_CHARACTERS + 1),
            "ERR|input_too_large",
        ),
        (
            "single-unicode-string-at-limit",
            format!("'{}'", "😀".repeat(MAX_STRING_CHARACTERS)),
            "OK|2",
        ),
        (
            "single-unicode-string-over-limit",
            format!("'{}'", "😀".repeat(MAX_STRING_CHARACTERS + 1)),
            "ERR|string_too_large",
        ),
        (
            "template-preserved-escape-at-limit",
            format!("`{}`", "\\q".repeat(MAX_STRING_CHARACTERS / 2)),
            "OK|2",
        ),
        (
            "template-preserved-escape-over-limit",
            format!("`{}`", "\\{".repeat(MAX_STRING_CHARACTERS / 2 + 1)),
            "ERR|string_too_large",
        ),
        (
            "long-unterminated-comment",
            format!("/*{}", " ".repeat(MAX_CHARACTERS - 2)),
            "ERR|unterminated_block_comment",
        ),
        (
            "long-overflowing-integer",
            "9".repeat(MAX_CHARACTERS),
            "ERR|integer_overflow",
        ),
    ]
}

struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before Unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("ku-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("create bootstrap stage-1 temp tree");
        Self(path)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ku_binary() -> PathBuf {
    if let Ok(path) = env::var("KU_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_ku") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    let executable = if cfg!(windows) { "ku.exe" } else { "ku" };
    let target = env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("target"));
    [
        target.join("debug").join(executable),
        target.join("release").join(executable),
        repo_root().join("target").join("debug").join(executable),
        repo_root().join("target").join("release").join(executable),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("ku binary not found; set KU_BIN or build it first")
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> BoundedOutput {
    let description = format!("{command:?}");
    run_bounded(command, timeout, BOOTSTRAP_OUTPUT_LIMITS).unwrap_or_else(|error| {
        panic!("bootstrap command did not complete safely: {description}\n{error}")
    })
}

fn combined(output: &BoundedOutput) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn normalized_stdout(output: &BoundedOutput) -> String {
    String::from_utf8_lossy(&output.stdout).replace('\r', "")
}

fn assert_success(label: &str, output: &BoundedOutput) {
    assert!(
        output.status.success(),
        "{label} failed with {:?}:\n{}",
        output.status.code(),
        combined(output)
    );
}

fn assert_canonical_output(label: &str, actual: &str, expected: &str) {
    if actual == expected {
        return;
    }
    // Never dump two entire repository corpora on the first mismatch.
    let actual_lines: Vec<_> = actual.split('\n').collect();
    let expected_lines: Vec<_> = expected.split('\n').collect();
    let mut case = "<before first case>";
    for index in 0..actual_lines.len().max(expected_lines.len()) {
        let actual_line = actual_lines.get(index).copied();
        let expected_line = expected_lines.get(index).copied();
        if let Some(line) = expected_line.filter(|line| line.starts_with("CASE|")) {
            case = line;
        }
        if actual_line != expected_line {
            let shorten = |line: Option<&str>| {
                line.map(|text| text.chars().take(512).collect::<String>())
                    .unwrap_or_else(|| "<missing>".into())
            };
            panic!(
                "{label}: first mismatch in {case}, output line {}; actual {} bytes, expected {} bytes\nactual: {:?}\nexpected: {:?}",
                index + 1,
                actual.len(),
                expected.len(),
                shorten(actual_line),
                shorten(expected_line)
            );
        }
    }
    unreachable!("unequal strings must differ in at least one split line");
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn boundary_driver() -> String {
    // MSVC rejects a single generated C string literal near 32 KiB.  Assemble
    // the exact character limits from portable 4 KiB literals instead.
    let input_chunk = " ".repeat(4_096);
    let string_chunk = "a".repeat(4_096);
    let token_chunk = "0 ".repeat(1_024);
    let token_tail = "0 ".repeat(1_023);
    let at_limit_expression = ["input_chunk.clone()"; 8].join(" + ");
    let over_limit_expression = format!("\"@\" + {}", ["input_chunk.clone()"; 8].join(" + "));
    let tokens_at_limit_expression =
        format!("{} + token_tail", ["token_chunk.clone()"; 3].join(" + "));
    let tokens_over_limit_expression = ["token_chunk.clone()"; 4].join(" + ");
    let mut driver = format!(
        r#"import {{ Scan, Canonical }} from "./lexer.ku"

fn Probe(label: str, source: str): null! {{
    println("BOUNDARY|" + label)
    try {{
        tokens = Scan(source)?
        println("OK|" + str(tokens.len()))
        if (tokens.len() == 4096) {{
            text = Canonical(tokens)
            println("CANONICAL|" + str(text.len()))
        }}
    }} catch(err) {{
        println("ERR|" + err.code)
    }}
    return ok(null)
}}

fn main(): null! {{
    input_chunk = "{input_chunk}"
    string_chunk = "{string_chunk}"
    token_chunk = "{token_chunk}"
    token_tail = "{token_tail}"
    input_at_limit = {at_limit_expression}
    input_over_limit = {over_limit_expression}
    string_at_limit = "\"" + string_chunk.clone() + "\""
    string_over_limit = "\"" + string_chunk + "a\""
    tokens_at_limit = {tokens_at_limit_expression}
    tokens_over_limit = {tokens_over_limit_expression}
    Probe("input-at-limit", input_at_limit)?
    Probe("input-over-limit", input_over_limit)?
    Probe("string-at-limit", string_at_limit)?
    Probe("string-over-limit", string_over_limit)?
    Probe("tokens-at-limit", tokens_at_limit)?
    Probe("tokens-over-limit", tokens_over_limit)?
    return ok(null)
}}
"#
    );
    let insertion = driver
        .rfind("    return ok(null)\n}\n")
        .expect("generated boundary main return");
    let mut extra = String::new();
    for (label, source, _) in additional_boundaries() {
        extra.push_str(&format!(
            "    Probe({}, {})?\n",
            quote_ku(label),
            ku_string_expression(&source)
        ));
    }
    driver.insert_str(insertion, &extra);
    driver
}

#[test]
fn bootstrap_stage1_ku_lexer_is_canonical_and_source_free() {
    let tree = TempTree::new("bootstrap-stage1");
    let source_dir = tree.0.join("src");
    fs::create_dir_all(&source_dir).expect("create copied source directory");
    let checked_in = repo_root().join("bootstrap").join("stage1");

    for name in ["token.ku", "lexer.ku", "main.ku"] {
        let text = fs::read_to_string(checked_in.join(name))
            .unwrap_or_else(|error| panic!("read bootstrap/stage1/{name}: {error}"));
        assert!(
            !text.contains("lexer.scan") && !text.contains("parser.parse"),
            "{name} must implement scanning in Ku instead of calling compiler builtins"
        );
        if name == "lexer.ku" {
            assert!(
                !text.contains("source.slice(") && !text.contains("fn PushToken("),
                "lexer collection must not rescan prefixes or copy a growing token array through a wrapper"
            );
            assert_eq!(
                text.matches("source.chars()").count(),
                1,
                "Unicode scalars must be materialized once"
            );
        }
        fs::write(source_dir.join(name), text)
            .unwrap_or_else(|error| panic!("copy bootstrap/stage1/{name}: {error}"));
    }

    let cases = differential_cases();
    let expected = oracle(&cases);
    fs::write(
        source_dir.join("differential.ku"),
        differential_driver(&cases),
    )
    .expect("write generated differential driver");
    fs::write(source_dir.join("boundary.ku"), boundary_driver())
        .expect("write generated bootstrap boundary driver");
    let expected_boundaries = boundary_expected();

    let ku = ku_binary();
    let check = run_with_timeout(
        Command::new(&ku)
            .current_dir(&source_dir)
            .args(["check", "main.ku"]),
        Duration::from_secs(20),
    );
    assert_success("ku check", &check);

    let demo = run_with_timeout(
        Command::new(&ku)
            .current_dir(&source_dir)
            .args(["run", "main.ku"]),
        Duration::from_secs(20),
    );
    assert_success("lexer example interpreter", &demo);
    assert_canonical_output(
        "lexer example",
        &normalized_stdout(&demo),
        &oracle(&demo_cases()),
    );

    let started = Instant::now();
    let interpreted = run_with_timeout(
        Command::new(&ku)
            .current_dir(&source_dir)
            .args(["run", "differential.ku"]),
        Duration::from_secs(60),
    );
    let interpreter_elapsed = started.elapsed();
    assert_success("differential interpreter", &interpreted);
    assert_canonical_output(
        "Ku lexer versus Rust kinds/payloads/spans/diagnostics",
        &normalized_stdout(&interpreted),
        &expected,
    );
    eprintln!(
        "bootstrap differential interpreter: {} cases in {interpreter_elapsed:?}",
        cases.len()
    );

    // These are full scans, not just direct guard calls: 32768 whitespace
    // scalars, decoded strings and 4095 tokens plus EOF (also canonicalized).
    let boundaries = run_with_timeout(
        Command::new(&ku)
            .current_dir(&source_dir)
            .args(["run", "boundary.ku"]),
        Duration::from_secs(60),
    );
    assert_success("bootstrap boundary interpreter", &boundaries);
    assert_canonical_output(
        "interpreter resource boundaries",
        &normalized_stdout(&boundaries),
        &expected_boundaries,
    );

    // C emission remains a hard gate even on a host without a C compiler.
    let emit = run_with_timeout(
        Command::new(&ku)
            .current_dir(&source_dir)
            .args(["build", "--native", "differential.ku"]),
        Duration::from_secs(30),
    );
    assert_success("native C emission", &emit);
    let c = fs::read_to_string(source_dir.join("differential.c")).expect("read emitted C artifact");
    assert!(
        c.lines().any(|line| {
            line.starts_with("KuResult_array_struct_") && line.contains("_Scan(KuString source)")
        }),
        "C artifact omitted the typed Scan entry"
    );
    assert!(
        c.contains("static KuArray_str ku_string_chars(KuString s)"),
        "C artifact omitted the one-pass Unicode scalar materializer"
    );
    assert!(
        c.lines()
            .any(|line| line.contains("ku_array_push_reuse_struct_") && line.contains("&tokens,")),
        "token collection did not lower to local storage reuse"
    );
    assert!(
        c.contains("ku_string_concat_reuse(&output,"),
        "canonical output did not lower to compound string append"
    );
    for guard in [
        "input_too_large",
        "too_many_tokens",
        "string_too_large",
        "32768",
        "4096",
    ] {
        assert!(c.contains(guard), "C artifact omitted lexer bound {guard}");
    }
    assert!(!c.contains("run_source"), "C artifact embedded the runner");
    assert!(
        !c.contains("const SOURCE"),
        "C artifact embedded a source runner"
    );

    let built_name = executable_name("stage1-built");
    let build = run_with_timeout(
        Command::new(&ku).current_dir(&tree.0).args([
            "build",
            "--native",
            "src/differential.ku",
            "-o",
            &built_name,
        ]),
        Duration::from_secs(90),
    );
    if !build.status.success() {
        let diagnostic = combined(&build);
        if diagnostic.contains("C compiler not found") {
            eprintln!("skip native executable gate: no C compiler available");
            return;
        }
        panic!("native executable build failed:\n{diagnostic}");
    }

    let boundary_built_name = executable_name("stage1-boundary-built");
    let boundary_build = run_with_timeout(
        Command::new(&ku).current_dir(&tree.0).args([
            "build",
            "--native",
            "src/boundary.ku",
            "-o",
            &boundary_built_name,
        ]),
        Duration::from_secs(90),
    );
    assert_success("native boundary executable build", &boundary_build);

    let relocated_dir = tree.0.join("relocated");
    fs::create_dir_all(&relocated_dir).expect("create relocated binary directory");
    let relocated = relocated_dir.join(executable_name("stage1-lexer"));
    let relocated_boundary = relocated_dir.join(executable_name("stage1-boundaries"));
    fs::copy(tree.0.join(&built_name), &relocated).expect("copy native executable");
    fs::copy(tree.0.join(&boundary_built_name), &relocated_boundary)
        .expect("copy native boundary executable");
    fs::remove_dir_all(&source_dir).expect("remove copied Ku sources before native run");
    assert!(!source_dir.exists(), "copied source directory must be gone");
    assert!(
        fs::read_dir(&relocated_dir)
            .expect("read relocated binary directory")
            .all(|entry| entry
                .expect("read relocated binary entry")
                .path()
                .extension()
                .is_none_or(|extension| extension != "ku")),
        "relocated binary directory must not contain Ku source files"
    );

    let native_boundaries = run_with_timeout(
        Command::new(&relocated_boundary).current_dir(&relocated_dir),
        Duration::from_secs(20),
    );
    assert_success("relocated native boundary executable", &native_boundaries);
    assert_canonical_output(
        "native resource boundaries",
        &normalized_stdout(&native_boundaries),
        &expected_boundaries,
    );

    let started = Instant::now();
    let native = run_with_timeout(
        Command::new(&relocated).current_dir(&relocated_dir),
        Duration::from_secs(20),
    );
    let native_elapsed = started.elapsed();
    assert_success("relocated native lexer executable", &native);
    assert_canonical_output(
        "source-free native versus Rust kinds/payloads/spans/diagnostics",
        &normalized_stdout(&native),
        &expected,
    );
    eprintln!(
        "bootstrap source-free native differential: {} cases in {native_elapsed:?}",
        cases.len()
    );
}

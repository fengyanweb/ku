use std::collections::HashSet;

use ku::{
    checker::Checker,
    error::{DiagnosticId, KuError, DIAGNOSTIC_REGISTRY},
    lexer::Lexer,
    parser::Parser,
    span::{Position, Span},
};

fn check_error(source: &str) -> KuError {
    let tokens = Lexer::new(source).tokenize().expect("fixture lexes");
    let program = Parser::new(tokens).parse_program().expect("fixture parses");
    if program
        .items
        .iter()
        .any(|item| matches!(item, ku::ast::Item::Import(_)))
    {
        // The public CLI checking pipeline expands imports before checking.
        // A bare Checker does not enable std modules from unresolved imports.
        return ku::cli::check_source("diagnostic-fixture.ku", source)
            .expect_err("fixture fails checking after import expansion");
    }
    Checker::new()
        .check(&program)
        .expect_err("fixture fails checking")
}

#[test]
fn diagnostic_registry_has_one_definition_per_id_and_code() {
    let mut ids = HashSet::new();
    let mut codes = HashSet::new();
    for entry in DIAGNOSTIC_REGISTRY {
        assert!(ids.insert(entry.id), "duplicate ID: {:?}", entry.id);
        assert!(codes.insert(entry.code), "duplicate code: {}", entry.code);
        assert_eq!(entry.id.definition(), entry);
        assert_eq!(entry.code.len(), 5);
        assert!(entry.code.starts_with('E'));
        assert!(entry.code[1..].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(!entry.summary.trim().is_empty());
        if !matches!(
            entry.id,
            DiagnosticId::SyntaxError | DiagnosticId::RuntimeError
        ) {
            assert!(!entry.helps.is_empty(), "{} needs repair help", entry.code);
        }
        assert!(entry
            .notes
            .iter()
            .chain(entry.helps)
            .all(|text| !text.trim().is_empty()));
    }
}

#[test]
fn diagnostic_explicit_id_survives_message_changes_clone_and_context() {
    let span = Span::new(Position::new(2, 3, 8), Position::new(2, 7, 12));
    let mut error = KuError::parse("arbitrary text", span)
        .with_diagnostic_id(DiagnosticId::UnreachableMatchArm)
        .with_diagnostic_context("imported.ku", "\n  code");
    error.message = "type mismatch: unknown std module".to_string();
    let clone = error.clone();
    assert_eq!(clone.diagnostic_id(), DiagnosticId::UnreachableMatchArm);
    let data = clone.diagnostic_data("main.ku", "");
    assert_eq!(data.code, "E0502");
    assert_eq!(data.file, "imported.ku");
    assert_eq!(
        (data.line, data.column, data.end_line, data.end_column),
        (2, 3, 2, 7)
    );
    assert_eq!(data.level, "error");
    assert_eq!(data.message, error.message);
    assert_eq!(
        data.helps,
        DiagnosticId::UnreachableMatchArm.definition().helps
    );
}

#[test]
fn diagnostic_actual_http_producers_have_distinct_codes_and_repair_help() {
    let arity = check_error(
        "import \"std.http\"\nfn main() { app = http.service() app.get(\"/\", fn(req, res) { return http.text(\"bad\") }) }",
    );
    let capture = check_error(
        "import \"std.http\"\nfn main() { count = 0 app = http.service() app.get(\"/\", fn() { count = count + 1 return http.text(\"bad\") }) }",
    );
    // check_source currently wraps the original rendered diagnostic. Check its
    // original code too, not just the compatibility classification of that text.
    assert!(arity.message.starts_with("error[E0701]:"), "{arity}");
    assert!(capture.message.starts_with("error[E0703]:"), "{capture}");
    assert_eq!(
        arity.diagnostic_id(),
        DiagnosticId::HttpHandlerSignature,
        "{arity}"
    );
    assert_eq!(
        capture.diagnostic_id(),
        DiagnosticId::HttpCapturedMutation,
        "{capture}"
    );
    assert_eq!(arity.diagnostic_data("http.ku", "").code, "E0701");
    let capture_data = capture.diagnostic_data("http.ku", "");
    assert_eq!(capture_data.code, "E0703");
    assert!(capture_data
        .helps
        .iter()
        .any(|help| help.contains("shared mutable captures")));
}

#[test]
fn diagnostic_actual_match_producers_distinguish_missing_and_unreachable_arms() {
    let missing = check_error(
        "enum Choice { A B } fn main() { value = Choice.A text = match value { Choice.A => 1 } println(text) }",
    );
    let unreachable =
        check_error("fn main() { value = 1 text = match value { _ => 1 2 => 2 } println(text) }");
    assert_eq!(
        missing.diagnostic_id(),
        DiagnosticId::NonExhaustiveMatch,
        "{missing}"
    );
    assert_eq!(
        unreachable.diagnostic_id(),
        DiagnosticId::UnreachableMatchArm,
        "{unreachable}"
    );
    let missing = missing.diagnostic_data("match.ku", "");
    let unreachable = unreachable.diagnostic_data("match.ku", "");
    assert_eq!(missing.code, "E0501");
    assert_eq!(unreachable.code, "E0502");
    assert!(missing.helps.iter().any(|help| help.contains("missing")));
    assert!(unreachable
        .helps
        .iter()
        .any(|help| help.contains("unreachable")));

    for source in [
        "fn main() { text = match 1 { 1 => 1 1 => 2 _ => 3 } println(text) }",
        "enum Choice { A B } fn main() { text = match Choice.A { Choice.A => 1 Choice.A => 2 Choice.B => 3 } println(text) }",
    ] {
        let duplicate = check_error(source);
        assert_eq!(duplicate.diagnostic_id(), DiagnosticId::UnreachableMatchArm, "{duplicate}");
    }
}

#[test]
fn diagnostic_import_categories_do_not_reuse_unknown_module_code() {
    let cases = [
        ("unknown std module 'Task'", "E0601"),
        ("'local' is not exported by helper.ku", "E0604"),
        ("module has no exported function 'Absent'", "E0604"),
        ("std module 'http' must be imported before use", "E0605"),
        ("cannot resolve import path", "E0600"),
    ];
    for (message, code) in cases {
        let data = KuError::message(message).diagnostic_data("imports.ku", "");
        assert_eq!(data.code, code, "{message}");
        assert!(!data.helps.is_empty());
    }
    let missing = check_error("fn main() { app = http.service() }");
    assert_eq!(
        missing.diagnostic_id(),
        DiagnosticId::StdImportRequired,
        "{missing}"
    );
}

#[test]
fn diagnostic_package_domain_and_runtime_error_code_are_not_repurposed() {
    let error = KuError::package(
        "not_found",
        "unknown std module in package metadata",
        Span::default(),
    );
    assert_eq!(error.diagnostic_id(), DiagnosticId::PackageError);
    assert_eq!(error.domain.as_deref(), Some("package"));
    assert_eq!(error.code.as_deref(), Some("not_found"));
    assert_eq!(error.diagnostic_data("package.ku", "").code, "E0606");
}

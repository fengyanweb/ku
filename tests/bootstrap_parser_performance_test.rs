//! Allocation and resource-closure scaling gate for native bootstrap parsers.
//!
//! Wall-clock time is diagnostic only. The enforced signals are generated
//! runtime allocation calls, requested bytes, peak live bytes, and a zero-live
//! invariant after every successful or structured-failure parse.

#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use native_allocation_harness::ALLOCATION_HOOK;
use native_pg_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

#[test]
fn native_bootstrap_parsers_close_allocations_and_scale_without_quadratic_copying() {
    let directory = TempDir::new("bootstrap-parser-performance");
    copy_bootstrap_sources(directory.path());
    let generated = emit_c(
        directory.path(),
        r#"import { Parse, ParseWithContext } from "./stage2/parser.ku"
import { ParseOutput } from "./stage2/ast.ku"
import { ParseContext } from "./stage2/context.ku"
import { Span } from "./stage2/span.ku"
import { ParseProgram } from "./stage3/parser.ku"

fn ParseWithInvalidDomain(source: str): ParseOutput! {
    owned_domain = source.clone()
    point = Span { line: 1, column: 1, offset: 0, end_line: 1, end_column: 1, end_offset: 0 }
    return ok(ParseWithContext("1", ParseContext {
        diagnostic_domain: owned_domain,
        origin: point.clone(),
        boundary: point,
        has_boundary: false
    })?)
}

fn main(): null! {
    return ok(null)
}
"#,
    );
    let abi = parser_native_abi(&generated);
    for required in [
        "typedef struct KuString {".to_string(),
        format!("static void {}(", abi.struct_drop),
        format!("static void {}(", abi.result_drop),
        format!(
            "if (result->ok) {{ {}(&result->value); }} else {{ ku_error_drop(&result->error); }}",
            abi.struct_drop
        ),
        "ku_string_drop(&error->domain);".to_string(),
        "ku_string_drop(&error->code);".to_string(),
        "ku_string_drop(&error->message);".to_string(),
        format!("{} {}(KuString source)", abi.result_type, abi.stage2_parse),
        format!("{} {}(KuString source)", abi.result_type, abi.stage3_parse),
        format!(
            "{} {}(KuString source)",
            abi.result_type, abi.invalid_domain_parse
        ),
    ] {
        assert!(
            generated.contains(&required),
            "generated parser C is missing lifecycle contract: {required}"
        );
    }

    let mut harness = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    let wrapper = PERFORMANCE_WRAPPER
        .replace("@KU_PARSE_RESULT@", &abi.result_type)
        .replace("@KU_PARSE_RESULT_DROP@", &abi.result_drop)
        .replace("@KU_STAGE2_PARSE@", &abi.stage2_parse)
        .replace("@KU_STAGE3_PARSE@", &abi.stage3_parse)
        .replace("@KU_INVALID_DOMAIN_PARSE@", &abi.invalid_domain_parse);
    harness.push_str(&wrapper);
    let source = directory.path().join("bootstrap-parser-performance.c");
    fs::write(&source, harness).expect("write bootstrap parser performance harness");
    let Some(executable) =
        compile_harness(directory.path(), &source, "bootstrap-parser-performance")
    else {
        return;
    };
    let mut command = Command::new(executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS).unwrap_or_else(|error| {
        panic!("bootstrap parser performance gate was not bounded: {error}")
    });
    assert!(
        output.status.success(),
        "bootstrap parser performance gate failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    for label in [
        "stage2-success",
        "stage2-wide-success",
        "stage2-failure",
        "stage3-success",
        "stage3-module-success",
        "stage3-named-import-success",
        "stage3-function-statements-success",
        "stage3-function-statements-failure",
        "stage3-if-statements-success",
        "stage3-if-statements-failure",
        "stage3-struct-success",
        "stage3-struct-union-success",
        "stage3-struct-failure",
        "stage3-enum-success",
        "stage3-enum-payload-success",
        "stage3-enum-failure",
        "stage3-failure",
        "stage3-expression-failure",
        "invalid-domain",
    ] {
        assert!(
            stdout.lines().any(|line| {
                line.starts_with(&format!("parser {label} rounds=8 "))
                    && line.contains(" small_allocs=")
                    && line.contains(" large_allocs=")
                    && line.contains(" small_total=")
                    && line.contains(" large_total=")
                    && line.contains(" small_peak=")
                    && line.contains(" large_peak=")
            }),
            "missing parser allocation metric for {label}:\n{stdout}"
        );
    }
    assert!(stdout.ends_with("bootstrap parser allocation closed loop\n"));
    eprint!("{stdout}");
    assert!(
        output.stderr.is_empty(),
        "unexpected parser benchmark stderr"
    );
}

struct ParserNativeAbi {
    result_type: String,
    result_drop: String,
    struct_drop: String,
    stage2_parse: String,
    stage3_parse: String,
    invalid_domain_parse: String,
}

fn parser_native_abi(generated: &str) -> ParserNativeAbi {
    let result_type = generated
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            if words.next() != Some("typedef") || words.next() != Some("struct") {
                return None;
            }
            let name = words.next()?;
            (name.starts_with("KuResult_struct_") && name.ends_with("ParseOutput"))
                .then(|| name.to_string())
        })
        .expect("generated C must declare ParseOutput Result ABI");
    let suffix = result_type
        .strip_prefix("KuResult_")
        .expect("ParseOutput result type prefix");
    ParserNativeAbi {
        result_drop: format!("ku_result_drop_{suffix}"),
        struct_drop: format!("ku_drop_{suffix}"),
        result_type,
        stage2_parse: generated_function(generated, "Parse"),
        stage3_parse: generated_function(generated, "ParseProgram"),
        invalid_domain_parse: generated_function(generated, "ParseWithInvalidDomain"),
    }
}

fn generated_function(generated: &str, source_name: &str) -> String {
    generated
        .lines()
        .find_map(|line| {
            let (head, _) = line.split_once("(KuString source) {")?;
            let name = head.split_whitespace().last()?;
            (name == source_name || name.ends_with(&format!("_{source_name}")))
                .then(|| name.to_string())
        })
        .unwrap_or_else(|| panic!("generated C must define {source_name}(KuString)"))
}

fn copy_bootstrap_sources(directory: &std::path::Path) {
    let bootstrap = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bootstrap");
    for (stage, files) in [
        ("stage1", &["token.ku", "lexer.ku"][..]),
        (
            "stage2",
            &[
                "span.ku",
                "diagnostic.ku",
                "ast.ku",
                "context.ku",
                "parser.ku",
            ][..],
        ),
        (
            "stage3",
            &[
                "support.ku",
                "signature.ku",
                "imports.ku",
                "structs.ku",
                "enums.ku",
                "functions.ku",
                "parser.ku",
            ][..],
        ),
    ] {
        let destination = directory.join(stage);
        fs::create_dir_all(&destination)
            .unwrap_or_else(|error| panic!("create bootstrap {stage} directory: {error}"));
        for file in files {
            fs::copy(bootstrap.join(stage).join(file), destination.join(file))
                .unwrap_or_else(|error| panic!("copy bootstrap {stage}/{file}: {error}"));
        }
    }
}

const PERFORMANCE_WRAPPER: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free

#define CHECK(value) do { if (!(value)) { fprintf(stderr, "check failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)
#define ROUNDS 8
#define KU_STRING_IS_ZERO(value) (!(value).ptr && !(value).len && !(value).capacity && !(value).storage)
#define KU_ARRAY_IS_ZERO(value) (!(value).data && !(value).len && !(value).capacity)
typedef @KU_PARSE_RESULT@ (*KuParserFunction)(KuString);
typedef struct {
  size_t max_calls, max_total, max_peak;
  unsigned long long elapsed_ms;
} KuParserMetric;
typedef struct {
  uint8_t* data;
  size_t len;
  unsigned long long hash;
} KuParserInput;

static unsigned long long ku_parser_fingerprint(const uint8_t* data, size_t len) {
  unsigned long long hash = 1469598103934665603ULL;
  for (size_t i = 0; i < len; i++) { hash ^= data[i]; hash *= 1099511628211ULL; }
  return hash;
}
static int ku_parser_string_equals(KuString value, const char* expected) {
  size_t len = strlen(expected);
  return value.len == len && (!len || !memcmp(value.ptr, expected, len));
}
static int ku_parser_string_starts_with(KuString value, const char* expected) {
  size_t len = strlen(expected);
  return value.len >= len && (!len || !memcmp(value.ptr, expected, len));
}
static int ku_parser_string_contains(KuString value, const char* expected) {
  size_t len = strlen(expected);
  if (!len) return 1;
  if (value.len < len) return 0;
  for (size_t i = 0; i <= value.len - len; i++) {
    if (!memcmp(value.ptr + i, expected, len)) return 1;
  }
  return 0;
}
static KuParserInput ku_stage2_input(size_t terms, int failure) {
  KuParserInput input = {0};
  input.len = terms * 2 - (failure ? 0 : 1);
  input.data = (uint8_t*)malloc(input.len ? input.len : 1);
  if (!input.data) return input;
  size_t cursor = 0;
  for (size_t i = 0; i < terms; i++) {
    input.data[cursor++] = '1';
    if (i + 1 < terms || failure) input.data[cursor++] = '+';
  }
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage2_array_input(size_t items) {
  KuParserInput input = {0};
  input.len = items * 2 + 1;
  input.data = (uint8_t*)malloc(input.len ? input.len : 1);
  if (!input.data) return input;
  size_t cursor = 0;
  input.data[cursor++] = '[';
  for (size_t i = 0; i < items; i++) {
    input.data[cursor++] = '1';
    if (i + 1 < items) input.data[cursor++] = ',';
  }
  input.data[cursor++] = ']';
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_input(size_t items, int failure) {
  static const char prefix[] = "fn main() { return [";
  static const char success_suffix[] = "]\n}";
  static const char failure_suffix[] = "]\n";
  const char* suffix = failure ? failure_suffix : success_suffix;
  size_t prefix_len = sizeof(prefix) - 1, suffix_len = strlen(suffix);
  KuParserInput input = {0};
  input.len = prefix_len + items * 2 - 1 + suffix_len;
  input.data = (uint8_t*)malloc(input.len ? input.len : 1);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < items; i++) {
    input.data[cursor++] = '1';
    if (i + 1 < items) input.data[cursor++] = ',';
  }
  memcpy(input.data + cursor, suffix, suffix_len);
  cursor += suffix_len;
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_expression_failure_input(size_t items) {
  static const char prefix[] = "fn main() { value = [";
  static const char suffix[] = "] + ;\n}";
  size_t prefix_len = sizeof(prefix) - 1, suffix_len = sizeof(suffix) - 1;
  KuParserInput input = {0};
  input.len = prefix_len + items * 2 - 1 + suffix_len;
  input.data = (uint8_t*)malloc(input.len ? input.len : 1);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < items; i++) {
    input.data[cursor++] = '1';
    if (i + 1 < items) input.data[cursor++] = ',';
  }
  memcpy(input.data + cursor, suffix, suffix_len);
  cursor += suffix_len;
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_function_statements_input(size_t statements, int failure) {
  static const char prefix[] = "fn main(){\n";
  static const char statement[] = "x=1\n";
  static const char suffix[] = "}\n";
  size_t prefix_len = sizeof(prefix) - 1, statement_len = sizeof(statement) - 1;
  size_t suffix_len = failure ? 0 : sizeof(suffix) - 1;
  size_t fixed_len = prefix_len + suffix_len;
  KuParserInput input = {0};
  if (!statements || statements > (SIZE_MAX - fixed_len) / statement_len) return input;
  input.len = fixed_len + statements * statement_len;
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < statements; i++) {
    memcpy(input.data + cursor, statement, statement_len);
    cursor += statement_len;
  }
  if (!failure) {
    memcpy(input.data + cursor, suffix, suffix_len);
    cursor += suffix_len;
  }
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_if_statements_input(size_t statements, int failure) {
  static const char prefix[] = "fn main(){if(true){\n";
  static const char statement[] = "x=1\n";
  static const char suffix[] = "}\n}\n";
  size_t prefix_len = sizeof(prefix) - 1, statement_len = sizeof(statement) - 1;
  size_t suffix_len = failure ? 0 : sizeof(suffix) - 1;
  size_t fixed_len = prefix_len + suffix_len;
  KuParserInput input = {0};
  if (!statements || statements > (SIZE_MAX - fixed_len) / statement_len) return input;
  input.len = fixed_len + statements * statement_len;
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < statements; i++) {
    memcpy(input.data + cursor, statement, statement_len);
    cursor += statement_len;
  }
  if (!failure) {
    memcpy(input.data + cursor, suffix, suffix_len);
    cursor += suffix_len;
  }
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_module_input(size_t items) {
  static const char item[] = "module M\n";
  size_t item_len = sizeof(item) - 1;
  KuParserInput input = {0};
  if (items && item_len > SIZE_MAX / items) return input;
  input.len = item_len * items;
  input.data = (uint8_t*)malloc(input.len ? input.len : 1);
  if (!input.data) return input;
  for (size_t i = 0; i < items; i++) memcpy(input.data + i * item_len, item, item_len);
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_named_import_input(size_t names) {
  static const char prefix[] = "import {";
  static const char entry[] = "A as B";
  static const char suffix[] = "} from \"./types.ku\";\n";
  size_t prefix_len = sizeof(prefix) - 1, entry_len = sizeof(entry) - 1;
  size_t suffix_len = sizeof(suffix) - 1;
  size_t fixed_len = prefix_len + suffix_len - 1;
  KuParserInput input = {0};
  if (!names || names > (SIZE_MAX - fixed_len) / (entry_len + 1)) return input;
  input.len = fixed_len + names * (entry_len + 1);
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < names; i++) {
    memcpy(input.data + cursor, entry, entry_len);
    cursor += entry_len;
    if (i + 1 < names) input.data[cursor++] = ',';
  }
  memcpy(input.data + cursor, suffix, suffix_len);
  cursor += suffix_len;
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_struct_input(size_t items) {
  static const char item[] = "struct S { value: int }\n";
  size_t item_len = sizeof(item) - 1;
  KuParserInput input = {0};
  if (items && item_len > SIZE_MAX / items) return input;
  input.len = item_len * items;
  input.data = (uint8_t*)malloc(input.len ? input.len : 1);
  if (!input.data) return input;
  size_t cursor = 0;
  for (size_t i = 0; i < items; i++) {
    memcpy(input.data + cursor, item, item_len);
    cursor += item_len;
  }
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_struct_union_input(size_t arms) {
  static const char prefix[] = "struct S { value: ";
  static const char suffix[] = " }\n";
  size_t prefix_len = sizeof(prefix) - 1, suffix_len = sizeof(suffix) - 1;
  size_t fixed_len = prefix_len + suffix_len - 1;
  KuParserInput input = {0};
  if (!arms || arms > (SIZE_MAX - fixed_len) / 2) return input;
  input.len = fixed_len + arms * 2;
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < arms; i++) {
    input.data[cursor++] = 'T';
    if (i + 1 < arms) input.data[cursor++] = '|';
  }
  memcpy(input.data + cursor, suffix, suffix_len);
  cursor += suffix_len;
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_struct_failure_input(size_t fields) {
  static const char prefix[] = "struct S { ";
  static const char field[] = "value:T|U ";
  size_t prefix_len = sizeof(prefix) - 1, field_len = sizeof(field) - 1;
  KuParserInput input = {0};
  if (!fields || fields > (SIZE_MAX - prefix_len) / field_len) return input;
  input.len = prefix_len + fields * field_len;
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < fields; i++) {
    memcpy(input.data + cursor, field, field_len);
    cursor += field_len;
  }
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_enum_input(size_t items) {
  static const char item[] = "enum E { A }\n";
  size_t item_len = sizeof(item) - 1;
  KuParserInput input = {0};
  if (items && item_len > SIZE_MAX / items) return input;
  input.len = item_len * items;
  input.data = (uint8_t*)malloc(input.len ? input.len : 1);
  if (!input.data) return input;
  size_t cursor = 0;
  for (size_t i = 0; i < items; i++) {
    memcpy(input.data + cursor, item, item_len);
    cursor += item_len;
  }
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_enum_payload_input(size_t fields) {
  static const char prefix[] = "enum E { V(";
  static const char field[] = "v:T|U";
  static const char suffix[] = ") }\n";
  size_t prefix_len = sizeof(prefix) - 1, field_len = sizeof(field) - 1;
  size_t suffix_len = sizeof(suffix) - 1;
  size_t fixed_len = prefix_len + suffix_len - 1;
  KuParserInput input = {0};
  if (!fields || fields > (SIZE_MAX - fixed_len) / (field_len + 1)) return input;
  input.len = fixed_len + fields * (field_len + 1);
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < fields; i++) {
    memcpy(input.data + cursor, field, field_len);
    cursor += field_len;
    if (i + 1 < fields) input.data[cursor++] = ',';
  }
  memcpy(input.data + cursor, suffix, suffix_len);
  cursor += suffix_len;
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_stage3_enum_failure_input(size_t variants) {
  static const char prefix[] = "enum E { ";
  static const char variant[] = "V(v:T),";
  size_t prefix_len = sizeof(prefix) - 1, variant_len = sizeof(variant) - 1;
  KuParserInput input = {0};
  if (!variants || variants > (SIZE_MAX - prefix_len) / variant_len) return input;
  input.len = prefix_len + variants * variant_len;
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  memcpy(input.data, prefix, prefix_len);
  size_t cursor = prefix_len;
  for (size_t i = 0; i < variants; i++) {
    memcpy(input.data + cursor, variant, variant_len);
    cursor += variant_len;
  }
  if (cursor != input.len) { free(input.data); return (KuParserInput){0}; }
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static KuParserInput ku_invalid_domain_input(size_t bytes) {
  static const char marker[] = "attacker-owned-domain";
  KuParserInput input = {0};
  if (bytes < sizeof(marker) - 1) return input;
  input.len = bytes;
  input.data = (uint8_t*)malloc(input.len);
  if (!input.data) return input;
  for (size_t i = 0; i < input.len; i++) input.data[i] = (uint8_t)('a' + i % 26);
  memcpy(input.data, marker, sizeof(marker) - 1);
  input.hash = ku_parser_fingerprint(input.data, input.len);
  return input;
}
static int ku_measure_parser(
    KuParserFunction parser, KuParserInput input, int succeeds,
    const char* domain, const char* code, const char* message,
    KuParserMetric* metric) {
  memset(metric, 0, sizeof(*metric));
  unsigned long long started = __ku_handler_now_ms();
  for (int round = 0; round < ROUNDS; round++) {
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
    ku_perf_calls = 0; ku_perf_total_bytes = 0; ku_perf_peak_bytes = 0; ku_perf_overflow = 0;
    KuString source = { input.data, input.len, 0, KU_STRING_STATIC };
    @KU_PARSE_RESULT@ parsed = parser(source);
    if (succeeds) {
      CHECK(parsed.ok && parsed.value.root > 0
          && (size_t)parsed.value.root == parsed.value.arena.nodes.len);
      CHECK(KU_STRING_IS_ZERO(parsed.error.domain)
          && KU_STRING_IS_ZERO(parsed.error.code)
          && KU_STRING_IS_ZERO(parsed.error.message));
    } else {
      CHECK(!parsed.ok && !parsed.value.root
          && KU_ARRAY_IS_ZERO(parsed.value.arena.nodes)
          && KU_ARRAY_IS_ZERO(parsed.value.arena.edges));
      CHECK(ku_parser_string_equals(parsed.error.domain, domain));
      CHECK(ku_parser_string_equals(parsed.error.code, code));
      CHECK(ku_parser_string_starts_with(parsed.error.message, message));
    }
    @KU_PARSE_RESULT_DROP@(&parsed);
    CHECK(!parsed.ok && !parsed.value.root
        && KU_ARRAY_IS_ZERO(parsed.value.arena.nodes)
        && KU_ARRAY_IS_ZERO(parsed.value.arena.edges)
        && KU_STRING_IS_ZERO(parsed.error.domain)
        && KU_STRING_IS_ZERO(parsed.error.code)
        && KU_STRING_IS_ZERO(parsed.error.message));
    CHECK(ku_parser_fingerprint(input.data, input.len) == input.hash);
    CHECK(!ku_perf_overflow && !ku_perf_live_allocations && !ku_perf_live_bytes);
    if (ku_perf_calls > metric->max_calls) metric->max_calls = ku_perf_calls;
    if (ku_perf_total_bytes > metric->max_total) metric->max_total = ku_perf_total_bytes;
    if (ku_perf_peak_bytes > metric->max_peak) metric->max_peak = ku_perf_peak_bytes;
  }
  unsigned long long finished = __ku_handler_now_ms();
  metric->elapsed_ms = finished >= started ? finished - started : 0;
  return 0;
}
static int ku_measure_invalid_domain(
    KuParserFunction parser, KuParserInput input, KuParserMetric* metric) {
  static const char domain[] = "bootstrap.parser";
  static const char code[] = "invalid_parse_context";
  static const char message[] = "error|bootstrap.parser|invalid_parse_context|<source>|parse diagnostic domain is not permitted|1:1@0..1:1@0";
  static const char reflected[] = "attacker-owned-domain";
  memset(metric, 0, sizeof(*metric));
  unsigned long long started = __ku_handler_now_ms();
  for (int round = 0; round < ROUNDS; round++) {
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
    ku_perf_calls = 0; ku_perf_total_bytes = 0; ku_perf_peak_bytes = 0; ku_perf_overflow = 0;
    KuString source = ku_string_alloc(input.len);
    CHECK(source.storage == KU_STRING_OWNED && source.len == input.len);
    if (input.len) memcpy(source.ptr, input.data, input.len);
    @KU_PARSE_RESULT@ parsed = parser(source);
    source = (KuString){0};
    CHECK(!parsed.ok && !parsed.value.root
        && KU_ARRAY_IS_ZERO(parsed.value.arena.nodes)
        && KU_ARRAY_IS_ZERO(parsed.value.arena.edges));
    CHECK(ku_parser_string_equals(parsed.error.domain, domain));
    CHECK(ku_parser_string_equals(parsed.error.code, code));
    CHECK(ku_parser_string_equals(parsed.error.message, message));
    CHECK(!ku_parser_string_contains(parsed.error.domain, reflected)
        && !ku_parser_string_contains(parsed.error.code, reflected)
        && !ku_parser_string_contains(parsed.error.message, reflected));
    @KU_PARSE_RESULT_DROP@(&parsed);
    CHECK(!parsed.ok && !parsed.value.root
        && KU_ARRAY_IS_ZERO(parsed.value.arena.nodes)
        && KU_ARRAY_IS_ZERO(parsed.value.arena.edges)
        && KU_STRING_IS_ZERO(parsed.error.domain)
        && KU_STRING_IS_ZERO(parsed.error.code)
        && KU_STRING_IS_ZERO(parsed.error.message));
    CHECK(ku_parser_fingerprint(input.data, input.len) == input.hash);
    CHECK(!ku_perf_overflow && !ku_perf_live_allocations && !ku_perf_live_bytes);
    if (ku_perf_calls > metric->max_calls) metric->max_calls = ku_perf_calls;
    if (ku_perf_total_bytes > metric->max_total) metric->max_total = ku_perf_total_bytes;
    if (ku_perf_peak_bytes > metric->max_peak) metric->max_peak = ku_perf_peak_bytes;
  }
  unsigned long long finished = __ku_handler_now_ms();
  metric->elapsed_ms = finished >= started ? finished - started : 0;
  return 0;
}
static int ku_scale_upper(
    size_t larger, size_t smaller, size_t numerator, size_t denominator,
    size_t allowance) {
  if (!numerator || !denominator || smaller > (SIZE_MAX - allowance) / numerator) return 0;
  return larger <= smaller * numerator / denominator + allowance;
}
static int ku_check_scale_bounded(
    const char* label, KuParserFunction parser,
    KuParserInput small, KuParserInput large, int succeeds,
    const char* domain, const char* code, const char* message,
    size_t numerator, size_t denominator, size_t call_allowance,
    size_t total_allowance, size_t peak_allowance) {
  KuParserMetric a = {0}, b = {0};
  CHECK(small.data && large.data && small.len < large.len);
  CHECK(ku_measure_parser(parser, small, succeeds, domain, code, message, &a) == 0);
  CHECK(ku_measure_parser(parser, large, succeeds, domain, code, message, &b) == 0);
  CHECK(a.max_calls && a.max_total && a.max_peak);
  CHECK(b.max_calls >= a.max_calls && b.max_total >= a.max_total && b.max_peak >= a.max_peak);
  printf("parser %s rounds=%d small_allocs=%zu large_allocs=%zu small_total=%zu large_total=%zu small_peak=%zu large_peak=%zu elapsed_ms=%llu/%llu\n",
      label, ROUNDS, a.max_calls, b.max_calls, a.max_total, b.max_total,
      a.max_peak, b.max_peak, a.elapsed_ms, b.elapsed_ms);
  CHECK(ku_scale_upper(b.max_calls, a.max_calls, numerator, denominator, call_allowance));
  CHECK(ku_scale_upper(b.max_total, a.max_total, numerator, denominator, total_allowance));
  CHECK(ku_scale_upper(b.max_peak, a.max_peak, numerator, denominator, peak_allowance));
  free(small.data); free(large.data);
  return 0;
}
static int ku_check_scale(
    const char* label, KuParserFunction parser,
    KuParserInput small, KuParserInput large, int succeeds,
    const char* domain, const char* code, const char* message) {
  return ku_check_scale_bounded(label, parser, small, large, succeeds,
      domain, code, message, 3, 1, 64, 256 * 1024, 128 * 1024);
}
static int ku_check_invalid_domain_scale(
    KuParserFunction parser, KuParserInput small, KuParserInput large) {
  KuParserMetric a = {0}, b = {0};
  CHECK(small.data && large.data && small.len * 2 == large.len);
  CHECK(ku_measure_invalid_domain(parser, small, &a) == 0);
  CHECK(ku_measure_invalid_domain(parser, large, &b) == 0);
  CHECK(a.max_calls && a.max_total && a.max_peak);
  CHECK(b.max_calls >= a.max_calls && b.max_total >= a.max_total && b.max_peak >= a.max_peak);
  printf("parser invalid-domain rounds=%d small_allocs=%zu large_allocs=%zu small_total=%zu large_total=%zu small_peak=%zu large_peak=%zu elapsed_ms=%llu/%llu\n",
      ROUNDS, a.max_calls, b.max_calls, a.max_total, b.max_total,
      a.max_peak, b.max_peak, a.elapsed_ms, b.elapsed_ms);
  CHECK(ku_scale_upper(b.max_calls, a.max_calls, 3, 1, 64));
  CHECK(ku_scale_upper(b.max_total, a.max_total, 3, 1, 256 * 1024));
  CHECK(ku_scale_upper(b.max_peak, a.max_peak, 3, 1, 128 * 1024));
  free(small.data); free(large.data);
  return 0;
}
int main(void) {
  CHECK(ku_check_scale("stage2-success", @KU_STAGE2_PARSE@,
      ku_stage2_input(128, 0), ku_stage2_input(256, 0), 1, "", "", "") == 0);
  CHECK(ku_check_scale("stage2-wide-success", @KU_STAGE2_PARSE@,
      ku_stage2_array_input(96), ku_stage2_array_input(192), 1, "", "", "") == 0);
  CHECK(ku_check_scale("stage2-failure", @KU_STAGE2_PARSE@,
      ku_stage2_input(128, 1), ku_stage2_input(256, 1), 0,
      "bootstrap.parser", "unexpected_eof",
      "error|bootstrap.parser|unexpected_eof|<source>|expected expression|") == 0);
  CHECK(ku_check_scale("stage3-success", @KU_STAGE3_PARSE@,
      ku_stage3_input(96, 0), ku_stage3_input(192, 0), 1, "", "", "") == 0);
  CHECK(ku_check_scale("stage3-module-success", @KU_STAGE3_PARSE@,
      ku_stage3_module_input(96), ku_stage3_module_input(192), 1, "", "", "") == 0);
  /* Full-alias lists are the longest accepted named-import shape. The 32/64
     fixtures consume 134/262 tokens including semicolon and EOF, keeping the
     larger case below the independent 266-token inspection bound. A 2.25x
     gate rejects copying the growing ImportNameRead array on every pure push. */
  CHECK(ku_check_scale_bounded("stage3-named-import-success", @KU_STAGE3_PARSE@,
      ku_stage3_named_import_input(32), ku_stage3_named_import_input(64),
      1, "", "", "", 9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  /* A single function containing 32/64 flat assignments consumes 103/199
     tokens including EOF. This isolates the extracted function reader's
     statement loop: cloning its complete token window once per statement or
     copying the growing statement/arena arrays must exceed the 2.25x gate. */
  CHECK(ku_check_scale_bounded("stage3-function-statements-success", @KU_STAGE3_PARSE@,
      ku_stage3_function_statements_input(32, 0),
      ku_stage3_function_statements_input(64, 0), 1, "", "", "",
      9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  /* The matching unterminated functions contain 102/198 tokens including EOF.
     All statement nodes are allocated before the missing-RBrace diagnostic,
     so the same bound also covers cleanup of the reader's partial arena. */
  CHECK(ku_check_scale_bounded("stage3-function-statements-failure", @KU_STAGE3_PARSE@,
      ku_stage3_function_statements_input(32, 1),
      ku_stage3_function_statements_input(64, 1), 0,
      "bootstrap.parser.stage3", "unexpected_eof",
      "error|bootstrap.parser.stage3|unexpected_eof|<source>|expected '}' after function body|",
      9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  /* A braced if containing 32/64 assignments consumes 109/205 tokens including
     EOF. The 2.25x gate covers condition/body projection and flattening into
     postorder without permitting repeated copies of the growing body list. */
  CHECK(ku_check_scale_bounded("stage3-if-statements-success", @KU_STAGE3_PARSE@,
      ku_stage3_if_statements_input(32, 0),
      ku_stage3_if_statements_input(64, 0), 1, "", "", "",
      9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  /* The matching unterminated if bodies consume 107/203 tokens including EOF.
     All partial statement nodes exist before the structured diagnostic, so
     eight zero-live rounds exercise deterministic error cleanup as well. */
  CHECK(ku_check_scale_bounded("stage3-if-statements-failure", @KU_STAGE3_PARSE@,
      ku_stage3_if_statements_input(32, 1),
      ku_stage3_if_statements_input(64, 1), 0,
      "bootstrap.parser.stage3", "unexpected_eof",
      "error|bootstrap.parser.stage3|unexpected_eof|<source>|expected '}' after if body|",
      9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  /* Each one-field struct is seven tokens, three nodes and three edges once
     linked by Program. Thus 32/64 items consume 225/449 tokens including EOF,
     97/193 nodes and 96/192 edges, all strictly inside the Stage 3 gates.
     The tighter 2.5x bound rejects cloning the complete token table once per
     struct while retaining small fixed allowances for allocator bookkeeping. */
  CHECK(ku_check_scale_bounded("stage3-struct-success", @KU_STAGE3_PARSE@,
      ku_stage3_struct_input(32), ku_stage3_struct_input(64), 1, "", "", "",
      5, 2, 64, 64 * 1024, 32 * 1024) == 0);
  /* A single 16/32-arm custom union consumes 38/70 tokens including EOF,
     20/36 nodes and 19/35 edges. This isolates type-plan flattening from the
     many-declaration token-window gate above; its 2.25x bound retains only
     small fixed allowances so repeated growing-array reads cannot hide. */
  CHECK(ku_check_scale_bounded("stage3-struct-union-success", @KU_STAGE3_PARSE@,
      ku_stage3_struct_union_input(16), ku_stage3_struct_union_input(32),
      1, "", "", "", 9, 4, 32, 8 * 1024, 4 * 1024) == 0);
  /* Missing '}' is detected only after all 32/64 complex fields have allocated
     their type and field nodes (164/324 tokens including EOF). The 2.25x gate
     covers both successful allocation and structured-error cleanup paths. */
  CHECK(ku_check_scale_bounded("stage3-struct-failure", @KU_STAGE3_PARSE@,
      ku_stage3_struct_failure_input(32), ku_stage3_struct_failure_input(64),
      0, "bootstrap.parser.stage3", "unexpected_token",
      "error|bootstrap.parser.stage3|unexpected_token|<source>|expected '}' after struct fields|",
      9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  /* Each unit enum is five tokens. The 32/64 declaration fixtures therefore
     consume 161/321 tokens including EOF, so both exercise repeated enum
     window/arena integration without approaching the 512-token hard limit. */
  CHECK(ku_check_scale_bounded("stage3-enum-success", @KU_STAGE3_PARSE@,
      ku_stage3_enum_input(32), ku_stage3_enum_input(64), 1, "", "", "",
      5, 2, 64, 64 * 1024, 32 * 1024) == 0);
  /* One variant with 32/64 union-typed payload fields consumes 199/391 tokens
     including EOF. This isolates payload field/type-plan growth while leaving
     a strict margin below every Stage 3 token, node, edge, and work budget. */
  CHECK(ku_check_scale_bounded("stage3-enum-payload-success", @KU_STAGE3_PARSE@,
      ku_stage3_enum_payload_input(32), ku_stage3_enum_payload_input(64),
      1, "", "", "", 9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  /* Missing '}' is reached only after 32/64 payload-bearing variants have
     allocated their type, field, and variant nodes (228/452 tokens including
     EOF). The structured-error path must release the complete partial arena. */
  CHECK(ku_check_scale_bounded("stage3-enum-failure", @KU_STAGE3_PARSE@,
      ku_stage3_enum_failure_input(32), ku_stage3_enum_failure_input(64),
      0, "bootstrap.parser.stage3", "unexpected_token",
      "error|bootstrap.parser.stage3|unexpected_token|<source>|expected '}' after enum variants|",
      9, 4, 64, 16 * 1024, 8 * 1024) == 0);
  CHECK(ku_check_scale("stage3-failure", @KU_STAGE3_PARSE@,
      ku_stage3_input(96, 1), ku_stage3_input(192, 1), 0,
      "bootstrap.parser.stage3", "unexpected_eof",
      "error|bootstrap.parser.stage3|unexpected_eof|<source>|expected '}' after function body|") == 0);
  CHECK(ku_check_scale("stage3-expression-failure", @KU_STAGE3_PARSE@,
      ku_stage3_expression_failure_input(96), ku_stage3_expression_failure_input(192), 0,
      "bootstrap.parser.stage3", "unexpected_eof",
      "error|bootstrap.parser.stage3|unexpected_eof|<source>|expected expression|") == 0);
  CHECK(ku_check_invalid_domain_scale(@KU_INVALID_DOMAIN_PARSE@,
      ku_invalid_domain_input(1024), ku_invalid_domain_input(2048)) == 0);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  puts("bootstrap parser allocation closed loop");
  return 0;
}
"#;

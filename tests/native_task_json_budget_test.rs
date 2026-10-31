//! Pre-allocation and failure ownership checks for frame-local JSON helpers.
//!
//! The JSON parser, writer and move/drop implementations are the generated
//! product code. A test-only reservation endpoint makes each boundary
//! deterministic; this is requested-byte accounting, not an RSS measurement.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "JSON budget hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_json_prepays_allocation_peaks_and_releases_failed_owners() {
    // A public, already supported scalar encoder requests the real JSON
    // runtime. The appended harness then exercises its new local-owner host
    // functions without a service, database, worker or private parser.
    let source = r#"
import json from "std.json"
async fn main():null! {
    text=json.encode({counter:7})?
    println(text)
    return ok(null)
}
"#;
    let mut ast = Parser::new(Lexer::new(source).lex().expect("budget fixture lexes"))
        .parse_program()
        .expect("budget fixture parses");
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:json".into(),
            span: Default::default(),
        }),
    );
    Checker::new().check(&ast).expect("budget fixture checks");
    let native = task_lower::lower_program(&ast).expect("budget fixture lowers");
    let generated = c::generate_native_task_c_source(&native, &Default::default())
        .expect("budget fixture emits native C");
    assert!(generated.contains("ku_task_host_json_decode"));
    let instrumentation = format!("{ALLOCATION_HOOK}\n{ALLOCATION_GUARD}\n");
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    // Observe actual host prepayments. Nothing fabricates a successful JSON
    // value, substitutes the engine, or alters ownership cleanup.
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_driver_grow_charge(",
        "static uint32_t ku_task_driver_grow_charge(const KuTaskDriverTicketV1*,size_t);\nstatic uint32_t fixture_real_driver_grow_charge(",
    );
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    generated.push_str(C_MAIN);
    let directory = TempDir::new("native-task-json-budget");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).expect("write JSON budget C");
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the Task JSON allocation gate"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("JSON budget process is bounded");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-json-budget-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const ALLOCATION_GUARD: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"Task JSON budget line %d: %s\n",__LINE__,#c); abort(); } } while(0)
static int fixture_budget_active;
static size_t fixture_budget_base,fixture_budget_limit,fixture_budget_reserved;
static size_t fixture_alloc_attempts,fixture_fail_at,fixture_reserves,fixture_reconciles;
static int fixture_allocation_allowed(size_t bytes) {
  if(!fixture_budget_active)return 1;
  size_t live=ku_perf_live_bytes>fixture_budget_base?ku_perf_live_bytes-fixture_budget_base:0u;
  /* Realloc is charged as old plus new while the old buffer is still live.
   * Allocator bookkeeping and libc-private allocations are outside this hook. */
  CHECK(live<=fixture_budget_reserved && bytes<=fixture_budget_reserved-live);
  fixture_alloc_attempts++;
  return !fixture_fail_at || fixture_alloc_attempts!=fixture_fail_at;
}
#undef malloc
#undef calloc
#undef realloc
#undef free
static void* fixture_json_malloc(size_t bytes) {
  return fixture_allocation_allowed(bytes)?ku_perf_malloc(bytes):NULL;
}
static void* fixture_json_calloc(size_t count,size_t width) {
  if(width && count>SIZE_MAX/width)return NULL;
  return fixture_allocation_allowed(count*width)?ku_perf_calloc(count,width):NULL;
}
static void* fixture_json_realloc(void* old,size_t bytes) {
  if(!bytes){ku_perf_free(old);return NULL;}
  return fixture_allocation_allowed(bytes)?ku_perf_realloc(old,bytes):NULL;
}
#define malloc fixture_json_malloc
#define calloc fixture_json_calloc
#define realloc fixture_json_realloc
#define free ku_perf_free
"#;

const C_MAIN: &str = r#"
static KuTaskDriverTicketV1 fixture_ticket;
static KuTaskControlV1 fixture_control;
static uint32_t fixture_reconcile(void* raw) {
  CHECK(raw==&fixture_budget_active && fixture_budget_active);
  CHECK(!fixture_alloc_attempts && ku_perf_live_bytes==fixture_budget_base);
  fixture_reconciles++;
  /* Keep the entire prepayment until the returned owner is registered or
   * dropped. The test endpoint deliberately cannot refund a temporary. */
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_grow_charge(const KuTaskDriverTicketV1* ticket,size_t bytes) {
  if(!fixture_budget_active)return fixture_real_driver_grow_charge(ticket,bytes);
  CHECK(ticket==&fixture_ticket && fixture_reconciles);
  fixture_reserves++;
  CHECK(!fixture_alloc_attempts);
  if(fixture_budget_reserved>fixture_budget_limit
      || bytes>fixture_budget_limit-fixture_budget_reserved)return KU_TASK_DRIVER_LIMIT;
  fixture_budget_reserved+=bytes;
  return KU_TASK_DRIVER_OK;
}
static KuTaskAdapterHostV1 fixture_begin(size_t limit,size_t fail_at) {
  CHECK(!fixture_budget_active);
  fixture_budget_base=ku_perf_live_bytes;fixture_budget_limit=limit;
  fixture_budget_reserved=fixture_alloc_attempts=fixture_reserves=fixture_reconciles=0u;
  fixture_fail_at=fail_at;fixture_budget_active=1;
  KuTaskAdapterHostV1 host={0};
  host.ticket=&fixture_ticket;host.control=&fixture_control;
  host.allocation_host_bytes=sizeof(host);
  host.allocation_host_abi=KU_TASK_ALLOCATION_HOST_ABI_VERSION;
  host.allocation_context=&fixture_budget_active;host.reconcile_charge=fixture_reconcile;
  return host;
}
static void fixture_end(void) {
  CHECK(fixture_budget_active && !ku_perf_overflow);
  fixture_budget_active=0;
}
static KuString fixture_static(const char* text) {
  return ku_string_static((const uint8_t*)text,strlen(text));
}
static int fixture_equal(KuString string,const char* text) {
  size_t length=strlen(text);return string.len==length && (!length || !memcmp(string.ptr,text,length));
}
static void fixture_no_live(void) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
}
static KuValue fixture_parse(const char* text) {
  KuResult_kuvalue result=ku_json_parse(fixture_static(text));CHECK(result.ok);
  return ku_value_move(&result.value);
}
static void fixture_drop_result(KuResult_kuvalue* result) {
  if(result->ok)ku_value_drop(&result->value);else ku_error_drop(&result->error);
  *result=(KuResult_kuvalue){0};
}
static KuValue fixture_owned_text(const char* text) {
  size_t length=strlen(text);uint8_t* copy=(uint8_t*)malloc(length?length:1u);CHECK(copy);
  if(length)memcpy(copy,text,length);
  return ku_v_str((KuString){copy,length,length?length:1u,KU_STRING_OWNED});
}
static void fixture_decode_case(const char* text,int valid) {
  fixture_no_live();KuTaskAdapterHostV1 host=fixture_begin(SIZE_MAX,0u);
  KuResult_kuvalue result=ku_task_host_json_decode(&host,fixture_static(text));
  CHECK(result.ok==!!valid && fixture_reserves==1u);
  size_t grant=fixture_budget_reserved,attempts=fixture_alloc_attempts;
  CHECK(grant && attempts<256u);fixture_end();fixture_drop_result(&result);fixture_no_live();

  host=fixture_begin(grant-1u,0u);
  result=ku_task_host_json_decode(&host,fixture_static(text));
  CHECK(!result.ok && fixture_equal(result.error.code,"resource_limit"));
  CHECK(!fixture_alloc_attempts && !ku_perf_live_allocations);fixture_end();
  fixture_drop_result(&result);fixture_no_live();

  host=fixture_begin(grant,0u);
  result=ku_task_host_json_decode(&host,fixture_static(text));CHECK(result.ok==!!valid);
  CHECK(fixture_budget_reserved==grant);fixture_end();fixture_drop_result(&result);fixture_no_live();

  /* Fault every allocating call in the successful/ordinary-error path.
   * Error construction may make additional calls, which must also fit the
   * original prepayment and must leave no retained partial tree. */
  for(size_t fault=1u;fault<=attempts;fault++) {
    host=fixture_begin(grant,fault);
    result=ku_task_host_json_decode(&host,fixture_static(text));
    CHECK(!result.ok);fixture_end();fixture_drop_result(&result);fixture_no_live();
  }
}
static void fixture_encode_case(const char* text) {
  fixture_no_live();KuValue source=fixture_parse(text);
  size_t source_bytes=ku_perf_live_bytes,live=0;
  CHECK(ku_task_json_owned_charge(source,&live,0u)==KU_TASK_DRIVER_OK && live==source_bytes);
  KuTaskAdapterHostV1 host=fixture_begin(SIZE_MAX,0u);
  KuResult_str encoded=ku_task_host_json_encode(&host,source);CHECK(encoded.ok);
  size_t grant=fixture_budget_reserved,attempts=fixture_alloc_attempts;
  CHECK(grant && fixture_reserves==1u && attempts<256u);fixture_end();
  KuResult_kuvalue decoded=ku_json_parse(encoded.value);CHECK(decoded.ok);
  fixture_drop_result(&decoded);ku_string_drop(&encoded.value);
  CHECK(ku_perf_live_bytes==source_bytes);

  host=fixture_begin(grant-1u,0u);encoded=ku_task_host_json_encode(&host,source);
  CHECK(!encoded.ok && fixture_equal(encoded.error.code,"resource_limit") && !fixture_alloc_attempts);
  fixture_end();ku_error_drop(&encoded.error);CHECK(ku_perf_live_bytes==source_bytes);
  for(size_t fault=1u;fault<=attempts;fault++) {
    host=fixture_begin(grant,fault);encoded=ku_task_host_json_encode(&host,source);
    CHECK(!encoded.ok);fixture_end();ku_error_drop(&encoded.error);
    CHECK(ku_perf_live_bytes==source_bytes);
  }
  ku_value_drop(&source);fixture_no_live();
}
static void fixture_lookup_and_consuming_accessors(void) {
  fixture_no_live();KuValue root=fixture_parse("{\"data\":{\"message\":\"owned\"},\"counter\":2}");
  size_t root_bytes=ku_perf_live_bytes;
  KuTaskAdapterHostV1 host=fixture_begin(SIZE_MAX,0u);
  KuResult_kuvalue copy=ku_task_host_json_get(&host,root,fixture_static("data"));CHECK(copy.ok);
  size_t grant=fixture_budget_reserved,attempts=fixture_alloc_attempts;fixture_end();
  CHECK(copy.value.tag==KU_OBJECT && copy.value.as.o!=ku_object_get(root.as.o,fixture_static("data"))->as.o);
  fixture_drop_result(&copy);CHECK(ku_perf_live_bytes==root_bytes);
  host=fixture_begin(grant-1u,0u);copy=ku_task_host_json_get(&host,root,fixture_static("data"));
  CHECK(!copy.ok && !fixture_alloc_attempts);fixture_end();fixture_drop_result(&copy);
  CHECK(ku_perf_live_bytes==root_bytes);
  for(size_t fault=1u;fault<=attempts;fault++) {
    host=fixture_begin(grant,fault);copy=ku_task_host_json_get(&host,root,fixture_static("data"));
    CHECK(!copy.ok);fixture_end();fixture_drop_result(&copy);CHECK(ku_perf_live_bytes==root_bytes);
  }
  host=fixture_begin(SIZE_MAX,0u);copy=ku_task_host_json_get(&host,root,fixture_static("data"));
  CHECK(copy.ok);fixture_end();
  KuObject* original=ku_object_get(root.as.o,fixture_static("data"))->as.o;
  host=fixture_begin(0u,0u);
  KuResult_kuvalue taken=ku_task_host_json_take(&host,&root,fixture_static("data"));
  CHECK(taken.ok && root.tag==KU_NULL && taken.value.as.o==original && !fixture_alloc_attempts);
  fixture_end();fixture_drop_result(&taken);
  CHECK(ku_object_get(copy.value.as.o,fixture_static("message"))->as.s.len==5u);
  fixture_drop_result(&copy);fixture_no_live();

  root=fixture_parse("{\"held\":[1,2,3]}");host=fixture_begin(0u,0u);
  taken=ku_task_host_json_take(&host,&root,fixture_static("missing"));
  CHECK(!taken.ok && root.tag==KU_NULL && !fixture_alloc_attempts);
  CHECK(fixture_equal(taken.error.code,"resource_limit"));fixture_end();fixture_drop_result(&taken);fixture_no_live();

  root=fixture_parse("\"moved-string\"");const uint8_t* original_string=root.as.s.ptr;
  host=fixture_begin(0u,0u);KuResult_str string=ku_task_host_json_as_str(&host,&root);
  CHECK(string.ok && root.tag==KU_NULL && string.value.ptr==original_string && !fixture_alloc_attempts);
  fixture_end();ku_string_drop(&string.value);fixture_no_live();
  root=fixture_parse("[\"wrong-type\"]");host=fixture_begin(0u,0u);
  KuResult_int integer=ku_task_host_json_as_int(&host,&root);
  CHECK(!integer.ok && root.tag==KU_NULL && !fixture_alloc_attempts);
  fixture_end();ku_error_drop(&integer.error);fixture_no_live();
}
static void fixture_object_consumes_every_path(void) {
  KuString keys[]={fixture_static("same"),fixture_static("same"),fixture_static("nested")};
  size_t grant=0,attempts=0;
  for(size_t mode=0;mode<3u;mode++) {
    fixture_no_live();
    KuValue values[]={fixture_owned_text("replaced"),fixture_owned_text("retained"),fixture_parse("[\"中文\",null]")};
    KuTaskAdapterHostV1 host=fixture_begin(mode==0u?SIZE_MAX:mode==1u?grant:grant-1u,0u);
    KuResult_kuvalue object=ku_task_host_json_build_object(&host,keys,values,3u);
    if(!mode){grant=fixture_budget_reserved;attempts=fixture_alloc_attempts;CHECK(grant && attempts<256u);}
    CHECK(object.ok==(mode!=2u));
    for(size_t i=0;i<3u;i++)CHECK(values[i].tag==KU_NULL);
    if(object.ok) {
      CHECK(object.value.as.o->len==2u);
      KuValue* kept=ku_object_get(object.value.as.o,fixture_static("same"));
      CHECK(kept && kept->tag==KU_STR && fixture_equal(kept->as.s,"retained"));
    }else CHECK(!fixture_alloc_attempts);
    fixture_end();fixture_drop_result(&object);fixture_no_live();
  }
  for(size_t fault=1u;fault<=attempts;fault++) {
    KuValue values[]={fixture_owned_text("replaced"),fixture_owned_text("retained"),fixture_parse("[\"中文\",null]")};
    KuTaskAdapterHostV1 host=fixture_begin(grant,fault);
    KuResult_kuvalue object=ku_task_host_json_build_object(&host,keys,values,3u);
    CHECK(!object.ok);
    for(size_t i=0;i<3u;i++)CHECK(values[i].tag==KU_NULL);
    fixture_end();fixture_drop_result(&object);fixture_no_live();
  }
}
int main(void) {
  fixture_decode_case("{\"dup\":1,\"dup\":2,\"text\":\"中文😀\\u0000\\\"\\\\\\n\",\"array\":[true,null,[],{}],\"n\":1e20}",1);
  fixture_decode_case("{\"allocated\":[\"prefix\",{\"nested\":1}],\"bad\":}",0);
  fixture_decode_case("{\"unterminated\":\"prefix\\\"",0);
  fixture_decode_case("[1e9999]",0);
  fixture_decode_case("\"\xc0\x80\"",0);
  char nested[68];
  for(size_t i=0;i<32u;i++)nested[i]='[';nested[32]='0';
  for(size_t i=0;i<32u;i++)nested[33+i]=']';nested[65]=0;
  fixture_decode_case(nested,1);
  for(size_t i=0;i<33u;i++)nested[i]='[';nested[33]='0';
  for(size_t i=0;i<33u;i++)nested[34+i]=']';nested[67]=0;
  fixture_decode_case(nested,0);
  fixture_encode_case("{\"text\":\"中文😀\\u0000\\\"\\\\\\n\",\"items\":[true,null,{},[]],\"n\":1e20}");
  fixture_encode_case("1e308");
  fixture_encode_case("{\"a\":{},\"b\":1,\"c\":2,\"d\":3,\"e\":4,\"f\":5,\"g\":6,\"h\":7,\"i\":8}");
  fixture_lookup_and_consuming_accessors();fixture_object_consumes_every_path();
  fixture_no_live();puts("task-json-budget-ok");return 0;
}
"#;

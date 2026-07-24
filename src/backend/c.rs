use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use crate::{
    ast::{BinaryOp, UnaryOp},
    error::{KuError, KuResult},
    ir::{
        FunctionId, IrBlock, IrCallKind, IrEnumLayout, IrExpr, IrExprKind, IrFieldLayout,
        IrFunction, IrInst, IrLValue, IrProgram, IrStructLayout, IrTerminator, IrType,
    },
    span::Span,
};

thread_local! {
    /// Maps a `FunctionId` to the C symbol that fills a `KuClosure` `invoke`
    /// slot: a lifted closure body's own name, or a top-level function's
    /// generated `__thunk`. Populated at the start of `generate_c_source` so the
    /// free-standing `c_expr` MakeClosure codegen can resolve it (Stage 6a).
    static CLOSURE_INVOKE_SYMBOLS: RefCell<HashMap<usize, String>> =
        RefCell::new(HashMap::new());
}

fn closure_invoke_symbol(id: FunctionId) -> Option<String> {
    CLOSURE_INVOKE_SYMBOLS.with(|symbols| symbols.borrow().get(&id.0).cloned())
}

/// The stable suffix identifying a closure ABI by its signature, e.g. a nullary
/// closure returning int is `fn__to_int`, a `(int) -> int` closure is
/// `fn_int__to_int`. Used for both the `KuClosure_<suffix>` struct name and the
/// `c_type` mapping.
fn closure_signature_suffix(params: &[IrType], ret: &IrType) -> KuResult<String> {
    let mut suffix = String::from("fn");
    for param in params {
        suffix.push('_');
        suffix.push_str(&c_type_suffix(param)?);
    }
    suffix.push_str("__to_");
    suffix.push_str(&c_type_suffix(ret)?);
    Ok(suffix)
}

struct OwnedLocal {
    source_name: String,
    name: String,
    ty: IrType,
    is_param: bool,
    /// A borrowed view (e.g. `a[i]` / `s.f` reading an owned element) shares the
    /// container's heap pointer. It must be declared but never dropped, or it
    /// double-frees with the container at scope exit.
    borrowed: bool,
}

pub fn generate_c_source(program: &IrProgram) -> KuResult<String> {
    validate_layouts(program)?;
    let mut out = String::from(
        "#include <stdbool.h>\n#include <stdint.h>\n#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n#include <time.h>\n\n\
         typedef struct KuString {\n  uint8_t* ptr;\n  size_t len;\n  size_t capacity;\n  uint8_t storage;\n} KuString;\n\
         enum { KU_STRING_STATIC = 0, KU_STRING_OWNED = 1 };\n\
         static KuString ku_string_static(const uint8_t* ptr, size_t len) {\n  return (KuString){ (uint8_t*)ptr, len, 0, KU_STRING_STATIC };\n}\n\
         static KuString ku_string_clone(KuString value) {\n  if (!value.ptr || value.len == 0) return value;\n  if (value.storage == KU_STRING_STATIC) return value;\n  uint8_t* data = (uint8_t*)malloc(value.len);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  memcpy(data, value.ptr, value.len);\n  return (KuString){ data, value.len, value.len, KU_STRING_OWNED };\n}\n\
         static KuString ku_string_move(KuString* value) {\n  KuString moved = *value;\n  *value = (KuString){0};\n  return moved;\n}\n\
         static void ku_string_drop(KuString* value) {\n  if (!value) return;\n  if (value->storage == KU_STRING_OWNED && value->ptr) free(value->ptr);\n  *value = (KuString){0};\n}\n\
         static bool ku_string_equal(KuString left, KuString right) {\n  return left.len == right.len && (left.len == 0 || memcmp(left.ptr, right.ptr, left.len) == 0);\n}\n\
         static KuString ku_string_concat(KuString left, KuString right) {\n  size_t len = left.len + right.len;\n  uint8_t* data = (uint8_t*)malloc(len ? len : 1);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  if (left.len) memcpy(data, left.ptr, left.len);\n  if (right.len) memcpy(data + left.len, right.ptr, right.len);\n  return (KuString){ data, len, len, KU_STRING_OWNED };\n}\n\
         static char* ku_string_to_cstr(KuString value) {\n  char* data = (char*)malloc(value.len + 1);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  if (value.len) memcpy(data, value.ptr, value.len);\n  data[value.len] = '\\0';\n  return data;\n}\n\
         static KuString ku_string_from_int(int64_t value) {\n  char buf[24];\n  int n = snprintf(buf, sizeof(buf), \"%lld\", (long long)value);\n  if (n <= 0) return (KuString){0};\n  uint8_t* data = (uint8_t*)malloc((size_t)n);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  memcpy(data, buf, (size_t)n);\n  return (KuString){ data, (size_t)n, (size_t)n, KU_STRING_OWNED };\n}\n\
         static KuString ku_string_from_bool(bool value) {\n  return value ? ku_string_static((const uint8_t*)\"true\", 4) : ku_string_static((const uint8_t*)\"false\", 5);\n}\n\
         static size_t ku_string_char_len(KuString s) {\n  size_t count = 0;\n  for (size_t i = 0; i < s.len; i++) { if ((s.ptr[i] & 0xC0) != 0x80) count++; }\n  return count;\n}\n\
         static bool ku_bytes_find(KuString hay, KuString needle, size_t from, size_t* out) {\n  if (needle.len == 0) { *out = from; return true; }\n  if (needle.len > hay.len) return false;\n  for (size_t i = from; i + needle.len <= hay.len; i++) {\n    if (memcmp(hay.ptr + i, needle.ptr, needle.len) == 0) { *out = i; return true; }\n  }\n  return false;\n}\n\
         static bool ku_string_contains(KuString hay, KuString needle) {\n  size_t at; return ku_bytes_find(hay, needle, 0, &at);\n}\n\
         static bool ku_string_starts_with(KuString s, KuString prefix) {\n  return prefix.len <= s.len && (prefix.len == 0 || memcmp(s.ptr, prefix.ptr, prefix.len) == 0);\n}\n\
         static bool ku_string_ends_with(KuString s, KuString suffix) {\n  return suffix.len <= s.len && (suffix.len == 0 || memcmp(s.ptr + (s.len - suffix.len), suffix.ptr, suffix.len) == 0);\n}\n\
         static KuString ku_string_alloc(size_t len) {\n  if (len == 0) return (KuString){0};\n  uint8_t* data = (uint8_t*)malloc(len);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  return (KuString){ data, len, len, KU_STRING_OWNED };\n}\n\
         static KuString ku_string_replace(KuString s, KuString from, KuString to) {\n  if (from.len == 0) {\n    /* Rust inserts `to` at every char boundary, including both ends. */\n    size_t chars = ku_string_char_len(s);\n    size_t out_len = to.len * (chars + 1) + s.len;\n    KuString out = ku_string_alloc(out_len);\n    size_t o = 0, i = 0;\n    if (to.len) { memcpy(out.ptr + o, to.ptr, to.len); o += to.len; }\n    while (i < s.len) {\n      size_t j = i + 1;\n      while (j < s.len && (s.ptr[j] & 0xC0) == 0x80) j++;\n      memcpy(out.ptr + o, s.ptr + i, j - i); o += j - i;\n      if (to.len) { memcpy(out.ptr + o, to.ptr, to.len); o += to.len; }\n      i = j;\n    }\n    return out;\n  }\n  size_t count = 0, i = 0, at;\n  while (ku_bytes_find(s, from, i, &at)) { count++; i = at + from.len; }\n  if (count == 0) return ku_string_clone(s);\n  size_t out_len = s.len + count * to.len - count * from.len;\n  KuString out = ku_string_alloc(out_len);\n  size_t o = 0, prev = 0;\n  i = 0;\n  while (ku_bytes_find(s, from, i, &at)) {\n    if (at > prev) { memcpy(out.ptr + o, s.ptr + prev, at - prev); o += at - prev; }\n    if (to.len) { memcpy(out.ptr + o, to.ptr, to.len); o += to.len; }\n    prev = at + from.len;\n    i = prev;\n  }\n  if (s.len > prev) { memcpy(out.ptr + o, s.ptr + prev, s.len - prev); o += s.len - prev; }\n  return out;\n}\n\n\
         typedef struct KuError {\n  KuString domain;\n  KuString code;\n  KuString message;\n} KuError;\n\
         static KuError ku_error_make(KuString domain, KuString code, KuString message) {\n  return (KuError){ domain, code, message };\n}\n\
         static KuError ku_error_message(KuString message) {\n  return (KuError){ ku_string_static((const uint8_t*)\"ku\", 2), ku_string_static((const uint8_t*)\"error\", 5), message };\n}\n\
         static KuError ku_error_clone(KuError error) {\n  return (KuError){ ku_string_clone(error.domain), ku_string_clone(error.code), ku_string_clone(error.message) };\n}\n\
         static KuError ku_error_move(KuError* error) {\n  KuError moved = *error;\n  *error = (KuError){0};\n  return moved;\n}\n\
         static void ku_error_drop(KuError* error) {\n  if (!error) return;\n  ku_string_drop(&error->domain);\n  ku_string_drop(&error->code);\n  ku_string_drop(&error->message);\n  *error = (KuError){0};\n}\n\
         static int64_t ku_time_now_millis(void) {\n  struct timespec ts;\n  timespec_get(&ts, TIME_UTC);\n  return (int64_t)ts.tv_sec * 1000 + (int64_t)(ts.tv_nsec / 1000000);\n}\n\
         static int64_t ku_time_steady_millis(void) {\n  struct timespec ts;\n  timespec_get(&ts, TIME_UTC);\n  return (int64_t)ts.tv_sec * 1000 + (int64_t)(ts.tv_nsec / 1000000);\n}\n\
         static KuString ku_fs_read(KuString path) {\n  char* p = ku_string_to_cstr(path);\n  FILE* f = fopen(p, \"rb\");\n  if (!f) { fprintf(stderr, \"fs.read: cannot open file\\n\"); free(p); exit(1); }\n  free(p);\n  fseek(f, 0, SEEK_END);\n  long len = ftell(f);\n  fseek(f, 0, SEEK_SET);\n  if (len < 0) { fclose(f); fprintf(stderr, \"fs.read: ftell failed\\n\"); exit(1); }\n  uint8_t* data = (uint8_t*)malloc((size_t)len ? (size_t)len : 1);\n  if (!data) { fclose(f); fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  size_t got = fread(data, 1, (size_t)len, f);\n  fclose(f);\n  return (KuString){ data, got, (size_t)len ? (size_t)len : 1, KU_STRING_OWNED };\n}\n\
         static uint8_t ku_fs_write(KuString path, KuString content) {\n  char* p = ku_string_to_cstr(path);\n  FILE* f = fopen(p, \"wb\");\n  if (!f) { fprintf(stderr, \"fs.write: cannot open file\\n\"); free(p); exit(1); }\n  free(p);\n  if (content.len) fwrite(content.ptr, 1, content.len, f);\n  fclose(f);\n  return 0;\n}\n\
         static bool ku_fs_exists(KuString path) {\n  char* p = ku_string_to_cstr(path);\n  FILE* f = fopen(p, \"rb\");\n  bool e = (f != NULL);\n  if (f) fclose(f);\n  free(p);\n  return e;\n}\n\n",
    );
    // Stage 8a: winsock headers + auto-link pragma for the native HTTP server.
    // `winsock2.h` must precede any `windows.h`; we include neither elsewhere, so
    // top-of-file placement is safe. The `#pragma comment(lib, ...)` makes MSVC
    // link `ws2_32` without any command-line change.
    // The pg connection pool uses CRITICAL_SECTION/CONDITION_VARIABLE (pulled in with
    // winsock2.h → windows.h), so a pooled pg program needs this block too.
    if program_uses_http(program) || program_uses_redis(program) || program_uses_pg_pool(program) {
        out.push_str(
            "#if defined(_WIN32)\n\
             #ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n\
             #include <winsock2.h>\n#include <ws2tcpip.h>\n#include <process.h>\n\
             #if defined(_MSC_VER)\n#pragma comment(lib, \"ws2_32.lib\")\n#endif\n\
             #endif\n\n",
        );
    }
    // Call-depth guard: match the interpreter's MAX_CALL_DEPTH so deep/infinite
    // recursion reports "maximum function call depth exceeded" instead of a
    // native stack-overflow crash. Counted at every function/closure-body entry
    // (thunks are transparent adapters and do not count).
    // Thread-local so concurrent workers (Stage 8) never share a call-depth
    // counter; matches the interpreter's MAX_CALL_DEPTH default of 512.
    out.push_str(
        "#define KU_MAX_CALL_DEPTH 512\n\
         #if defined(_MSC_VER)\n#define KU_THREAD_LOCAL __declspec(thread)\n\
         #else\n#define KU_THREAD_LOCAL _Thread_local\n#endif\n\
         static KU_THREAD_LOCAL long __ku_call_depth = 0;\n\n",
    );
    // Aggregate struct fields (e.g. `[Person]`) need a layered emission so the
    // struct↔array cycle resolves: forward-declare every struct tag, then emit all
    // array typedefs (a `KuArray_KuStruct_X` only needs the struct as a pointer),
    // then the struct bodies (which can embed a `KuArray_*` by value), then the
    // ownership helpers (forward-declared so struct-clone↔array-clone can recurse).
    emit_struct_forward_decls(&mut out, program);
    emit_array_typedefs(&mut out, program)?;
    emit_array_helper_prototypes(&mut out, program)?;
    // Opaque libpq handle typedefs must precede the Result ABI (KuResult_pg_conn
    // embeds a PGconn* field).
    emit_pg_types(&mut out, program);
    emit_redis_types(&mut out, program);
    emit_mysql_types(&mut out, program);
    emit_layouts(&mut out, program)?;
    emit_named_ownership_helpers(&mut out, program)?;
    register_closure_invoke_symbols(program);
    // The env header carries the type-erased retain/release used by both closures
    // and the `KuValue` Function tag. Emit it up front whenever either feature is
    // present so the object ABI (which clones/drops Function values) and the
    // closure structs can all reference it, regardless of emission order below.
    let mut closure_types_present = Vec::new();
    collect_closure_types_program(program, &mut closure_types_present);
    let mut closure_header_done = false;
    if !closure_types_present.is_empty()
        || program_uses_object(program)
        || program_uses_http(program)
    {
        out.push_str(
            "typedef struct KuEnvHeader { void (*retain)(void*); void (*release)(void*); size_t rc; } KuEnvHeader;\n\n",
        );
        closure_header_done = true;
    }
    // Closure struct typedefs come in two passes around the aggregate ABIs so the
    // cyclic-looking (but acyclic) dependency between array-of-closures and
    // closure-returning-array is resolved by emission order (see `emit_closure_types`).
    let mut closure_emitted = std::collections::HashSet::new();
    emit_closure_types(&mut out, program, &mut closure_header_done, &mut closure_emitted, true)?;
    emit_array_abi(&mut out, program)?;
    emit_object_abi(&mut out, program)?;
    emit_http_types(&mut out, program)?;
    emit_result_abi(&mut out, program)?;
    emit_array_try_get_helpers(&mut out, program)?;
    emit_string_slice_helper(&mut out, program)?;
    emit_object_result_helpers(&mut out, program)?;
    emit_closure_types(&mut out, program, &mut closure_header_done, &mut closure_emitted, false)?;
    emit_closure_value_wrappers(&mut out, program)?;
    emit_cell_types(&mut out, program)?;
    emit_env_types(&mut out, program)?;
    emit_array_map_helpers(&mut out, program)?;
    emit_closure_body_prototypes(&mut out, program)?;
    emit_closure_thunk_prototypes(&mut out, program)?;
    emit_http_runtime(&mut out, program)?;
    emit_pg_runtime(&mut out, program);
    emit_redis_runtime(&mut out, program);
    emit_mysql_runtime(&mut out, program);
    for function in &program.functions {
        emit_function(&mut out, function)?;
        out.push('\n');
    }
    emit_closure_thunks(&mut out, program)?;
    emit_main_wrapper(&mut out, program)?;
    Ok(out)
}

/// Record every function's `KuClosure` `invoke` symbol so MakeClosure codegen can
/// resolve a `FunctionId` without threading the function table through `c_expr`.
fn register_closure_invoke_symbols(program: &IrProgram) {
    let mut symbols = HashMap::new();
    for function in &program.functions {
        let symbol = if function.is_closure_body {
            c_symbol(&function.name)
        } else {
            format!("{}__thunk", c_symbol(&function.name))
        };
        symbols.insert(function.id.0, symbol);
    }
    CLOSURE_INVOKE_SYMBOLS.with(|slot| *slot.borrow_mut() = symbols);
}

/// True when every type in a closure signature lowers to a self-contained C type
/// (no `KuArray_*`/`KuResult_*`/`KuObject`/nested closure). Such a signature can
/// be emitted before the array/result ABI, which is required so an
/// array-of-closures (`[fn(): int]`) sees `KuClosure_*` already defined. Closures
/// whose signature *does* reference those aggregates are emitted in the later
/// pass, after those ABIs exist.
fn closure_signature_is_self_contained(params: &[IrType], ret: &IrType) -> bool {
    params
        .iter()
        .chain(std::iter::once(ret))
        .all(|ty| matches!(ty, IrType::Int | IrType::Float | IrType::Bool | IrType::Str | IrType::Null | IrType::Void))
}

/// Emit a `typedef struct { ret (*invoke)(void*, params...); void* env; }` for
/// every distinct closure signature the program uses (Stage 6a). Runs in two
/// passes sharing `header_done`/`emitted`: `self_contained_only == true` emits
/// signatures over primitives before the array/result ABI (so array-of-closures
/// resolves `KuClosure_*`); the second pass (`false`) emits the remainder after
/// those ABIs exist (so a closure returning e.g. `[int]` sees `KuArray_int`).
fn emit_closure_types(
    out: &mut String,
    program: &IrProgram,
    header_done: &mut bool,
    emitted: &mut std::collections::HashSet<String>,
    self_contained_only: bool,
) -> KuResult<()> {
    let mut types = Vec::new();
    collect_closure_types_program(program, &mut types);
    let selected: Vec<&IrType> = types
        .iter()
        .filter(|ty| match ty {
            IrType::Closure { params, ret } => {
                closure_signature_is_self_contained(params, ret) == self_contained_only
            }
            _ => false,
        })
        .collect();
    if selected.is_empty() {
        return Ok(());
    }
    // Stage 6b: a shared env header so a closure can retain/release its env
    // without knowing the concrete `KuEnv_{id}` type (closures of one signature
    // may carry envs of different capture layouts). Every `KuEnv_{id}` begins
    // with these fields, so a `void* env` can be reached through this header.
    if !*header_done {
        out.push_str(
            "typedef struct KuEnvHeader { void (*retain)(void*); void (*release)(void*); size_t rc; } KuEnvHeader;\n",
        );
        *header_done = true;
    }
    for ty in selected {
        let IrType::Closure { params, ret } = ty else {
            continue;
        };
        let suffix = closure_signature_suffix(params, ret)?;
        if !emitted.insert(suffix.clone()) {
            continue;
        }
        let mut param_list = String::from("void*");
        for param in params {
            param_list.push_str(", ");
            param_list.push_str(&c_type(param)?);
        }
        out.push_str(&format!(
            "typedef struct {{ {} (*invoke)({}); void* env; }} KuClosure_{suffix};\n",
            c_type(ret)?,
            param_list,
        ));
        // Move transfers ownership of the env to the destination, nulling the
        // source so a later drop of the source is a no-op (single-owner).
        out.push_str(&format!(
            "static KuClosure_{suffix} ku_closure_move_{suffix}(KuClosure_{suffix}* c) {{ KuClosure_{suffix} moved = *c; c->env = NULL; return moved; }}\n"
        ));
        // Stage 6e-2: clone shares the captured environment by bumping its
        // refcount (the cells themselves are not copied). A NULL env (Stage 6a
        // no-capture closure) is a plain struct copy. The result is an
        // independent owner that releases its env exactly once when dropped.
        out.push_str(&format!(
            "static KuClosure_{suffix} ku_closure_clone_{suffix}(KuClosure_{suffix} c) {{ if (c.env) ((KuEnvHeader*)c.env)->retain(c.env); return c; }}\n"
        ));
    }
    out.push('\n');
    Ok(())
}

/// Stage 6b: emit a `KuCell_{suffix}` box plus new/retain/release for every Copy
/// payload type boxed anywhere in the program (discovered from `CellNew`).
fn emit_cell_types(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut inners: Vec<IrType> = Vec::new();
    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                if let IrInst::CellNew { ty, .. } = inst {
                    if !inners.contains(ty) {
                        inners.push(ty.clone());
                    }
                }
            }
        }
    }
    if inners.is_empty() {
        return Ok(());
    }
    for inner in &inners {
        let suffix = c_type_suffix(inner)?;
        let payload = c_type(inner)?;
        // Stage 6c: the cell owns its payload. `new` takes ownership of `init`;
        // the last `release` drops the payload exactly once, then frees the box.
        // Copy payloads (int/bool/null) need no payload drop.
        let drop_payload = match inner {
            IrType::Str => "ku_string_drop(&c->value); ".to_string(),
            IrType::Array(element) => {
                format!("ku_array_drop_{}(&c->value); ", c_type_suffix(element)?)
            }
            IrType::Named(name) if name == "__ku_object" => "ku_object_drop(c->value); ".to_string(),
            _ => String::new(),
        };
        out.push_str(&format!(
            "typedef struct {{ {payload} value; size_t rc; }} KuCell_{suffix};\n\
             static KuCell_{suffix}* ku_cell_{suffix}_new({payload} init) {{ KuCell_{suffix}* c = (KuCell_{suffix}*)malloc(sizeof(KuCell_{suffix})); if (!c) {{ fprintf(stderr, \"out of memory\\n\"); exit(1); }} c->value = init; c->rc = 1; return c; }}\n\
             static void ku_cell_{suffix}_retain(KuCell_{suffix}* c) {{ if (c) c->rc++; }}\n\
             static void ku_cell_{suffix}_release(KuCell_{suffix}* c) {{ if (c && --c->rc == 0) {{ {drop_payload}free(c); }} }}\n"
        ));
    }
    out.push('\n');
    Ok(())
}

/// Stage 6b: emit a `KuEnv_{id}` (with type-erased retain/release matching
/// `KuEnvHeader`) for every capturing closure body. The env holds one reference
/// per captured cell (retained on `new`, released on the env's final release).
fn emit_env_types(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut emitted = false;
    for function in &program.functions {
        if !function.is_closure_body || function.captures.is_empty() {
            continue;
        }
        let id = function.id.0;
        // Field declarations and the constructor parameter list.
        let mut fields = String::new();
        let mut params = String::new();
        let mut assigns = String::new();
        let mut releases = String::new();
        for (index, (name, ty)) in function.captures.iter().enumerate() {
            let IrType::Cell(inner) = ty else {
                return Err(unsupported("Stage 6b env capture must be a cell type"));
            };
            let suffix = c_type_suffix(inner)?;
            let ident = c_ident(name);
            fields.push_str(&format!("  KuCell_{suffix}* {ident};\n"));
            if index > 0 {
                params.push_str(", ");
            }
            params.push_str(&format!("KuCell_{suffix}* {ident}"));
            assigns.push_str(&format!(
                "  e->{ident} = {ident}; ku_cell_{suffix}_retain({ident});\n"
            ));
            releases.push_str(&format!("  ku_cell_{suffix}_release(e->{ident});\n"));
        }
        out.push_str(&format!(
            "typedef struct KuEnv_{id} {{\n  void (*retain)(void*);\n  void (*release)(void*);\n  size_t rc;\n{fields}}} KuEnv_{id};\n"
        ));
        out.push_str(&format!(
            "static void ku_env_{id}_retain(void* p) {{ KuEnv_{id}* e = (KuEnv_{id}*)p; if (e) e->rc++; }}\n"
        ));
        out.push_str(&format!(
            "static void ku_env_{id}_release(void* p) {{ KuEnv_{id}* e = (KuEnv_{id}*)p; if (e && --e->rc == 0) {{\n{releases}  free(e);\n}} }}\n"
        ));
        out.push_str(&format!(
            "static KuEnv_{id}* ku_env_{id}_new({params}) {{ KuEnv_{id}* e = (KuEnv_{id}*)malloc(sizeof(KuEnv_{id})); if (!e) {{ fprintf(stderr, \"out of memory\\n\"); exit(1); }} e->retain = ku_env_{id}_retain; e->release = ku_env_{id}_release; e->rc = 1;\n{assigns}  return e; }}\n"
        ));
        emitted = true;
    }
    if emitted {
        out.push('\n');
    }
    Ok(())
}

/// Forward-declare every lifted closure body so a `MakeClosure` can reference it
/// regardless of where the body sits in the emitted function order.
fn emit_closure_body_prototypes(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut emitted = false;
    for function in &program.functions {
        if !function.is_closure_body {
            continue;
        }
        out.push_str(&closure_body_signature(function)?);
        out.push_str(";\n");
        emitted = true;
    }
    if emitted {
        out.push('\n');
    }
    Ok(())
}

/// The C signature of a lifted closure body (leading `void* __env`), matching
/// what `emit_function` emits so the forward declaration and definition agree.
fn closure_body_signature(function: &IrFunction) -> KuResult<String> {
    let mut params = String::from("void* __env");
    for param in &function.params {
        params.push_str(", ");
        params.push_str(&format!("{} {}", c_type(&param.ty)?, c_ident(&param.name)));
    }
    Ok(format!(
        "{} {}({})",
        c_type(&function.return_type)?,
        c_symbol(&function.name),
        params
    ))
}

/// Forward-declare the adapter thunks so a `MakeClosure` for a top-level function
/// can be emitted before that thunk's body (which follows all functions).
fn emit_closure_thunk_prototypes(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let targets = closure_thunk_targets(program);
    if targets.is_empty() {
        return Ok(());
    }
    for function in &targets {
        out.push_str(&thunk_signature(function)?);
        out.push_str(";\n");
    }
    out.push('\n');
    Ok(())
}

/// Emit the adapter thunk bodies: `ret name__thunk(void* __env, params) { (void)__env; return name(args); }`.
fn emit_closure_thunks(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let targets = closure_thunk_targets(program);
    for function in &targets {
        out.push_str(&thunk_signature(function)?);
        out.push_str(" {\n  (void)__env;\n");
        let args = function
            .params
            .iter()
            .map(|param| c_ident(&param.name))
            .collect::<Vec<_>>()
            .join(", ");
        if function.return_type == IrType::Void {
            out.push_str(&format!("  {}({});\n", c_symbol(&function.name), args));
        } else {
            out.push_str(&format!("  return {}({});\n", c_symbol(&function.name), args));
        }
        out.push_str("}\n");
    }
    if !targets.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn thunk_signature(function: &IrFunction) -> KuResult<String> {
    let mut params = String::from("void* __env");
    for param in &function.params {
        params.push_str(", ");
        params.push_str(&format!("{} {}", c_type(&param.ty)?, c_ident(&param.name)));
    }
    Ok(format!(
        "static {} {}__thunk({})",
        c_type(&function.return_type)?,
        c_symbol(&function.name),
        params
    ))
}

/// Top-level functions referenced by a `MakeClosure` need a thunk adapter.
fn closure_thunk_targets(program: &IrProgram) -> Vec<&IrFunction> {
    let mut ids = Vec::new();
    collect_make_closure_ids_program(program, &mut ids);
    program
        .functions
        .iter()
        .filter(|function| !function.is_closure_body && ids.contains(&function.id))
        .collect()
}

fn collect_make_closure_ids_program(program: &IrProgram, ids: &mut Vec<FunctionId>) {
    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                walk_inst_exprs(inst, &mut |expr| collect_make_closure_ids_expr(expr, ids));
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                collect_make_closure_ids_expr(expr, ids)
            });
        }
    }
}

fn collect_make_closure_ids_expr(expr: &IrExpr, ids: &mut Vec<FunctionId>) {
    if let IrExprKind::MakeClosure { function_id, .. } = &expr.kind {
        if !ids.contains(function_id) {
            ids.push(*function_id);
        }
    }
    for child in expr_children(expr) {
        collect_make_closure_ids_expr(child, ids);
    }
}

/// Stage 6f: emit one `ku_array_map_<sig>` helper per distinct mapper signature
/// the program's `arr.map` calls use. The signature suffix encodes the closure's
/// parameter (== the input element type) and return type (== the result element
/// type), so it uniquely names the helper. Runs after the array and closure ABIs
/// so both `KuArray_*` and `KuClosure_*` are already defined.
fn emit_array_map_helpers(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut calls = Vec::new();
    collect_array_map_calls_program(program, &mut calls);
    let mut emitted = std::collections::HashSet::new();
    for (in_element, params, ret) in calls {
        let cl_suffix = closure_signature_suffix(&params, &ret)?;
        if !emitted.insert(cl_suffix.clone()) {
            continue;
        }
        let in_array = c_array_type(&in_element)?;
        let out_array = c_array_type(&ret)?;
        let out_type = c_type(&ret)?;
        // Each element is cloned before being handed to the mapper (identity for
        // Copy types like int): the input array keeps ownership of its elements
        // while the closure body owns the value it receives.
        let arg = c_clone_value(&in_element, "array.data[index]")?;
        out.push_str(&format!(
            "static {out_array} ku_array_map_{cl_suffix}({in_array} array, KuClosure_{cl_suffix} mapper) {{\n\
             \x20 {out_array} result = {{ array.len, NULL }};\n\
             \x20 if (array.len > 0) {{\n\
             \x20   if (array.len > SIZE_MAX / sizeof({out_type})) {{ fprintf(stderr, \"array allocation is too large\\n\"); exit(1); }}\n\
             \x20   result.data = ({out_type}*)malloc(array.len * sizeof({out_type}));\n\
             \x20   if (!result.data) {{ fprintf(stderr, \"array allocation failed\\n\"); exit(1); }}\n\
             \x20   for (size_t index = 0; index < array.len; index++) {{\n\
             \x20     result.data[index] = mapper.invoke(mapper.env, {arg});\n\
             \x20   }}\n\
             \x20 }}\n\
             \x20 if (mapper.env) ((KuEnvHeader*)mapper.env)->release(mapper.env);\n\
             \x20 return result;\n\
             }}\n\n"
        ));
    }
    Ok(())
}

fn collect_array_map_calls_program(
    program: &IrProgram,
    calls: &mut Vec<(IrType, Vec<IrType>, IrType)>,
) {
    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                walk_inst_exprs(inst, &mut |expr| collect_array_map_calls_expr(expr, calls));
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                collect_array_map_calls_expr(expr, calls)
            });
        }
    }
}

fn collect_array_map_calls_expr(expr: &IrExpr, calls: &mut Vec<(IrType, Vec<IrType>, IrType)>) {
    if let IrExprKind::Call {
        kind: IrCallKind::Intrinsic(name),
        args,
        ..
    } = &expr.kind
    {
        if name == "array.map" {
            if let (Some(receiver), Some(mapper)) = (args.first(), args.get(1)) {
                if let (IrType::Array(element), IrType::Closure { params, ret }) =
                    (&receiver.ty, &mapper.ty)
                {
                    calls.push(((**element).clone(), params.clone(), (**ret).clone()));
                }
            }
        }
    }
    for child in expr_children(expr) {
        collect_array_map_calls_expr(child, calls);
    }
}

fn collect_closure_types_program(program: &IrProgram, types: &mut Vec<IrType>) {
    for function in &program.functions {
        collect_closure_type(&function.return_type, types);
        for param in &function.params {
            collect_closure_type(&param.ty, types);
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                walk_inst_types(inst, &mut |ty| collect_closure_type(ty, types));
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                collect_closure_type(&expr.ty, types)
            });
        }
    }
}

fn collect_closure_type(ty: &IrType, types: &mut Vec<IrType>) {
    match ty {
        IrType::Closure { params, ret } => {
            if !types.contains(ty) {
                types.push(ty.clone());
            }
            for param in params {
                collect_closure_type(param, types);
            }
            collect_closure_type(ret, types);
        }
        IrType::Array(inner) | IrType::Result(inner) => collect_closure_type(inner, types),
        _ => {}
    }
}

/// Visit every `IrExpr` referenced by an instruction and its closure types.
fn walk_inst_types(inst: &IrInst, visit: &mut dyn FnMut(&IrType)) {
    walk_inst_exprs(inst, &mut |expr| walk_expr_types(expr, visit));
    match inst {
        IrInst::Temp { ty, .. } | IrInst::BindOk { ty, .. } | IrInst::Let { ty, .. } => visit(ty),
        _ => {}
    }
}

fn walk_expr_types(expr: &IrExpr, visit: &mut dyn FnMut(&IrType)) {
    visit(&expr.ty);
    for child in expr_children(expr) {
        walk_expr_types(child, visit);
    }
}

fn walk_inst_exprs(inst: &IrInst, visit: &mut dyn FnMut(&IrExpr)) {
    match inst {
        IrInst::Temp { value, .. }
        | IrInst::BindOk { result: value, .. }
        | IrInst::Let { value, .. }
        | IrInst::Store { value, .. }
        | IrInst::Print(value)
        | IrInst::Expr(value)
        | IrInst::Fail(value)
        | IrInst::Panic(value) => visit(value),
        IrInst::CellNew { init, .. } => visit(init),
        IrInst::CellStore { cell, value } => {
            visit(cell);
            visit(value);
        }
        IrInst::BeginTry { .. }
        | IrInst::EndTry
        | IrInst::BindError { .. }
        | IrInst::DefineClosure { .. }
        | IrInst::CellRelease(_)
        | IrInst::Unsupported { .. } => {}
    }
}

fn walk_terminator_exprs(terminator: &IrTerminator, visit: &mut dyn FnMut(&IrExpr)) {
    match terminator {
        IrTerminator::Branch { condition, .. } => visit(condition),
        IrTerminator::ForEach { iterable, .. } => visit(iterable),
        IrTerminator::ResultBranch { result, .. }
        | IrTerminator::JumpErr { result, .. }
        | IrTerminator::PropagateErr(result)
        | IrTerminator::Return(Some(result)) => visit(result),
        IrTerminator::Next
        | IrTerminator::Jump(_)
        | IrTerminator::Return(None)
        | IrTerminator::Unreachable => {}
    }
}

fn expr_children(expr: &IrExpr) -> Vec<&IrExpr> {
    match &expr.kind {
        IrExprKind::Unary { expr, .. } | IrExprKind::TryUnwrap(expr) => vec![expr],
        IrExprKind::Binary { left, right, .. } => vec![left, right],
        IrExprKind::Call { callee, args, .. } => {
            let mut children = vec![callee.as_ref()];
            children.extend(args.iter());
            children
        }
        IrExprKind::Array(values) => values.iter().collect(),
        IrExprKind::Index { target, index } => vec![target, index],
        IrExprKind::Field { target, .. } => vec![target],
        IrExprKind::StructLiteral { fields, .. } => fields.iter().map(|(_, v)| v).collect(),
        IrExprKind::CellLoad(inner) => vec![inner],
        IrExprKind::Literal(_)
        | IrExprKind::Local(_)
        | IrExprKind::Temp(_)
        | IrExprKind::MakeClosure { .. }
        | IrExprKind::CapturedCell(_) => Vec::new(),
    }
}

fn validate_layouts(program: &IrProgram) -> KuResult<()> {
    let indexes = program
        .layouts
        .structs
        .iter()
        .enumerate()
        .map(|(index, layout)| (layout.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut dependency_count = vec![0usize; program.layouts.structs.len()];
    let mut dependents = vec![Vec::new(); program.layouts.structs.len()];

    for (index, layout) in program.layouts.structs.iter().enumerate() {
        for field in &layout.fields {
            match &field.ty {
                IrType::Int | IrType::Bool | IrType::Str => {}
                // Array fields (`[int]`, `[Person]`, `[[int]]`): the array typedef is
                // emitted before the struct layout and the array helpers are forward-
                // declared, so embedding one by value and deep clone/drop both work.
                // The element must itself be an "early" type (primitive/struct/nested
                // array of those); an array of closures/objects is not supported as a
                // struct field.
                IrType::Array(element) if is_early_array_element(element) => {}
                IrType::Array(_) => {
                    return Err(unsupported(format!(
                        "native C struct '{}.{}' supports array fields of int/bool/str/struct only for now",
                        layout.name, field.name
                    )));
                }
                IrType::Named(name) if enum_type_name(name).is_none() => {
                    let Some(&dependency) = indexes.get(name.as_str()) else {
                        return Err(unsupported(format!(
                            "native C struct '{}.{}' references unknown struct '{name}'",
                            layout.name, field.name
                        )));
                    };
                    dependency_count[index] += 1;
                    dependents[dependency].push(index);
                }
                // An enum field embeds the enum by value; `emit_layouts` emits the
                // enum typedef before this struct (unified struct+enum topological
                // order), so it is allowed here.
                IrType::Named(_) => {}
                other => {
                    return Err(unsupported(format!(
                        "native C struct '{}.{}' does not support field type {other}; supported field types are int, bool, str, and non-recursive named structs",
                        layout.name, field.name
                    )));
                }
            }
        }
    }

    let mut ready = dependency_count
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for dependent in &dependents[index] {
            dependency_count[*dependent] -= 1;
            if dependency_count[*dependent] == 0 {
                ready.push_back(*dependent);
            }
        }
    }
    if visited != program.layouts.structs.len() {
        return Err(unsupported(
            "native C prototype does not support recursive value struct layouts",
        ));
    }

    for (index, layout) in program.layouts.structs.iter().enumerate() {
        for field in &layout.fields {
            // Only struct-typed fields are ordered among the struct list here; enum
            // fields are ordered by `emit_layouts` across the unified struct+enum
            // graph, so skip them (their name is not in the struct `indexes`).
            if let IrType::Named(name) = &field.ty {
                if enum_type_name(name).is_some() {
                    continue;
                }
                let dependency = indexes[name.as_str()];
                if dependency >= index {
                    return Err(unsupported(format!(
                        "native C struct '{}.{}' must reference a struct declared earlier than '{}'; declaration-order value layouts cannot use a later struct",
                        layout.name, field.name, layout.name
                    )));
                }
            }
        }
    }

    let enum_indexes = program
        .layouts
        .enums
        .iter()
        .enumerate()
        .map(|(index, layout)| (layout.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    for (index, layout) in program.layouts.enums.iter().enumerate() {
        for variant in &layout.variants {
            for field in &variant.fields {
                match &field.ty {
                    IrType::Int | IrType::Bool | IrType::Str => {}
                    IrType::Named(name) if enum_type_name(name).is_none() => {
                        if !indexes.contains_key(name.as_str()) {
                            return Err(unsupported(format!(
                                "native C enum '{}.{}.{}' references unknown struct '{name}'",
                                layout.name, variant.name, field.name
                            )));
                        }
                    }
                    IrType::Named(name) => {
                        let enum_name = enum_type_name(name).expect("checked enum marker");
                        let Some(&dependency) = enum_indexes.get(enum_name) else {
                            return Err(unsupported(format!(
                                "native C enum '{}.{}.{}' references unknown enum '{enum_name}'",
                                layout.name, variant.name, field.name
                            )));
                        };
                        if dependency >= index {
                            return Err(unsupported(format!(
                                "native C enum '{}.{}.{}' must reference an enum declared earlier; recursive enum value layouts are not supported",
                                layout.name, variant.name, field.name
                            )));
                        }
                    }
                    other => {
                        return Err(unsupported(format!(
                            "native C enum '{}.{}.{}' does not support payload type {other}; supported payloads are int, bool, str, structs, and earlier enums",
                            layout.name, variant.name, field.name
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Emit struct and enum typedefs in a unified topological order: a type is emitted
/// after every type it embeds BY VALUE (a struct field or enum payload of `Named`
/// type). This lets a struct hold an enum field and an enum hold a struct payload
/// in the same program. Array fields embed through a `KuArray_*` pointer, so they
/// create no ordering dependency (their typedefs are emitted separately).
fn emit_layouts(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let structs = &program.layouts.structs;
    let enums = &program.layouts.enums;
    let n_structs = structs.len();
    let total = n_structs + enums.len();
    if total == 0 {
        return Ok(());
    }
    // Node ids: 0..n_structs are structs, n_structs..total are enums.
    let mut struct_index: HashMap<&str, usize> = HashMap::new();
    for (i, layout) in structs.iter().enumerate() {
        struct_index.insert(layout.name.as_str(), i);
    }
    let mut enum_index: HashMap<&str, usize> = HashMap::new();
    for (i, layout) in enums.iter().enumerate() {
        enum_index.insert(layout.name.as_str(), n_structs + i);
    }
    let node_of = |ty: &IrType| -> Option<usize> {
        if let IrType::Named(name) = ty {
            match enum_type_name(name) {
                Some(ename) => enum_index.get(ename).copied(),
                None => struct_index.get(name.as_str()).copied(),
            }
        } else {
            None
        }
    };

    let mut indegree = vec![0usize; total];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); total];
    for (i, layout) in structs.iter().enumerate() {
        for field in &layout.fields {
            if let Some(dep) = node_of(&field.ty) {
                indegree[i] += 1;
                dependents[dep].push(i);
            }
        }
    }
    for (i, layout) in enums.iter().enumerate() {
        let node = n_structs + i;
        for variant in &layout.variants {
            for field in &variant.fields {
                if let Some(dep) = node_of(&field.ty) {
                    indegree[node] += 1;
                    dependents[dep].push(node);
                }
            }
        }
    }

    // Kahn's algorithm, always taking the lowest ready node id for stable output
    // (independent structs keep declaration order, and precede independent enums).
    let mut done = vec![false; total];
    for _ in 0..total {
        let Some(node) = (0..total).find(|&n| !done[n] && indegree[n] == 0) else {
            return Err(unsupported(
                "native C prototype does not support recursive value struct/enum layouts",
            ));
        };
        done[node] = true;
        if node < n_structs {
            emit_struct_layout(out, &structs[node])?;
        } else {
            emit_enum_layout(out, &enums[node - n_structs])?;
        }
        for dependent in dependents[node].clone() {
            indegree[dependent] -= 1;
        }
    }
    out.push('\n');
    Ok(())
}

fn emit_struct_layout(out: &mut String, layout: &IrStructLayout) -> KuResult<()> {
    // The tag was forward-declared (and typedef'd) by `emit_struct_forward_decls`,
    // so complete the body here rather than re-typedef the name.
    let name = c_struct_type(&layout.name);
    out.push_str(&format!("struct {name} {{\n"));
    for field in &layout.fields {
        out.push_str(&format!(
            "  {} {};\n",
            c_type(&field.ty)?,
            c_ident(&field.name)
        ));
    }
    out.push_str("};\n");
    Ok(())
}

fn emit_enum_layout(out: &mut String, layout: &IrEnumLayout) -> KuResult<()> {
    let name = c_enum_type(&layout.name);
    out.push_str(&format!(
        "typedef struct {name} {{\n  int32_t tag;\n  union {{\n"
    ));
    let mut emitted_payload = false;
    for variant in &layout.variants {
        if variant.fields.is_empty() {
            continue;
        }
        emitted_payload = true;
        out.push_str("    struct {\n");
        for field in &variant.fields {
            out.push_str(&format!(
                "      {} {};\n",
                c_type(&field.ty)?,
                c_ident(&field.name)
            ));
        }
        out.push_str(&format!("    }} {};\n", c_ident(&variant.name)));
    }
    if !emitted_payload {
        out.push_str("    unsigned char empty;\n");
    }
    out.push_str(&format!("  }} payload;\n}} {name};\n"));
    Ok(())
}

/// Collect every array element type the program needs a `KuArray_*` for: from
/// function signatures and bodies, and from struct/enum field types (a `[int]`
/// field forces `KuArray_int` even if no bare `[int]` value appears in any body).
fn collect_all_array_elements(program: &IrProgram, element_types: &mut Vec<IrType>) {
    for function in &program.functions {
        collect_array_element_type(&function.return_type, element_types);
        for param in &function.params {
            collect_array_element_type(&param.ty, element_types);
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
                        collect_array_element_type(ty, element_types);
                        collect_array_expr_types(value, element_types);
                    }
                    IrInst::BindOk { ty, result, .. } => {
                        collect_array_element_type(ty, element_types);
                        collect_array_expr_types(result, element_types);
                    }
                    IrInst::Store { target, value } => {
                        collect_array_lvalue_types(target, element_types);
                        collect_array_expr_types(value, element_types);
                    }
                    IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value) => {
                        collect_array_expr_types(value, element_types);
                    }
                    IrInst::CellNew { init, .. } => {
                        collect_array_expr_types(init, element_types)
                    }
                    IrInst::CellStore { cell, value } => {
                        collect_array_expr_types(cell, element_types);
                        collect_array_expr_types(value, element_types);
                    }
                    IrInst::BeginTry { .. }
                    | IrInst::EndTry
                    | IrInst::BindError { .. }
                    | IrInst::DefineClosure { .. }
                    | IrInst::CellRelease(_)
                    | IrInst::Unsupported { .. } => {}
                }
            }
        }
    }
    for layout in &program.layouts.structs {
        for field in &layout.fields {
            collect_array_element_type(&field.ty, element_types);
        }
    }
    for layout in &program.layouts.enums {
        for variant in &layout.variants {
            for field in &variant.fields {
                collect_array_element_type(&field.ty, element_types);
            }
        }
    }
    // `ku_pg_query_params` / `ku_mysql_query_params` take a `KuArray_str`, so a pg or
    // mysql program needs that array ABI even if it never writes a `[str]` literal.
    if (program_uses_pg(program) || program_uses_mysql(program))
        && !element_types.contains(&IrType::Str)
    {
        element_types.push(IrType::Str);
    }
}

/// Forward-declare every user-struct tag so a `KuArray_KuStruct_X` typedef can hold
/// a `KuStruct_X*` before the struct body is emitted. The struct body later completes
/// the same tag (`struct KuStruct_X { ... };`), so `emit_struct_layout` must emit the
/// body form, not another `typedef`.
fn emit_struct_forward_decls(out: &mut String, program: &IrProgram) {
    for layout in &program.layouts.structs {
        let name = c_struct_type(&layout.name);
        out.push_str(&format!("typedef struct {name} {name};\n"));
    }
    if !program.layouts.structs.is_empty() {
        out.push('\n');
    }
}

/// An array element whose `KuArray_E` typedef is emittable *before* the struct
/// layouts: a primitive, a user struct (its tag is forward-declared, and the array
/// only holds a pointer to it), or a nested array of such. Closure/object/http/value
/// elements are NOT early — their C types are defined later, so those array typedefs
/// stay in `emit_array_abi` at their original position.
fn is_early_array_element(element: &IrType) -> bool {
    match element {
        IrType::Int | IrType::Bool | IrType::Str => true,
        IrType::Array(inner) => is_early_array_element(inner),
        IrType::Named(name) => {
            enum_type_name(name).is_none() && !name.starts_with("__ku_")
        }
        _ => false,
    }
}

/// Emit the `KuArray_E` typedefs whose element is "early" (see `is_early_array_element`)
/// before the struct layouts, plus the shared bounds-fail helper (emitted whenever any
/// array exists at all). Late-element typedefs are emitted by `emit_array_abi`.
fn emit_array_typedefs(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut element_types = Vec::new();
    collect_all_array_elements(program, &mut element_types);
    if element_types.is_empty() {
        return Ok(());
    }
    out.push_str(
        "static void ku_array_bounds_fail(int64_t index, size_t len) {\n  fprintf(stderr, \"array index %lld out of bounds for length %zu\\n\", (long long)index, len);\n  exit(1);\n}\n\n",
    );
    for element in &element_types {
        if !is_early_array_element(element) {
            continue;
        }
        let array_type = c_array_type(element)?;
        let element_type = c_type(element)?;
        out.push_str(&format!(
            "typedef struct {{ size_t len; {element_type}* data; }} {array_type};\n"
        ));
    }
    out.push('\n');
    Ok(())
}

/// Forward-declare the early-element array helpers that the struct ownership pass
/// calls (a struct's deep clone/drop invokes `ku_array_clone_*` / `ku_array_drop_*`
/// for its array fields), so those uses resolve before the helper bodies are emitted.
fn emit_array_helper_prototypes(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut element_types = Vec::new();
    collect_all_array_elements(program, &mut element_types);
    let mut any = false;
    for element in &element_types {
        if !is_early_array_element(element) {
            continue;
        }
        let array_type = c_array_type(element)?;
        let suffix = c_type_suffix(element)?;
        out.push_str(&format!(
            "static {array_type} ku_array_clone_{suffix}({array_type} array);\n\
             static void ku_array_drop_{suffix}({array_type}* array);\n"
        ));
        any = true;
    }
    if any {
        out.push('\n');
    }
    Ok(())
}

fn emit_array_abi(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut element_types = Vec::new();
    collect_all_array_elements(program, &mut element_types);
    // Early-element typedefs were emitted before the struct layouts; emit the
    // late-element typedefs (closure/object arrays) here, at the original position
    // after their C types are defined. Then emit every helper body — early-element
    // helpers can now call the struct/enum ownership helpers defined just above.
    for element in &element_types {
        if is_early_array_element(element) {
            continue;
        }
        let array_type = c_array_type(element)?;
        let element_type = c_type(element)?;
        out.push_str(&format!(
            "typedef struct {{ size_t len; {element_type}* data; }} {array_type};\n"
        ));
    }
    for element in &element_types {
        emit_array_helpers_for(out, element)?;
    }
    Ok(())
}

/// Emit the `KuArray_<suffix>` helper bodies (make/clone/move/drop/get/at/len/
/// is_empty/push) for one element type. The typedef is emitted separately by
/// `emit_array_typedefs`.
fn emit_array_helpers_for(out: &mut String, element: &IrType) -> KuResult<()> {
    let array_type = c_array_type(element)?;
    let suffix = c_type_suffix(element)?;
    let element_type = c_type(element)?;
    let clone_element = c_clone_value(element, "array.data[index]")?;
    let drop_element = c_drop_value(element, "array->data[index]")?;
    let clone_pushed = c_clone_value(element, "value")?;
    out.push_str(&format!(
        "static {array_type} ku_array_make_{suffix}(size_t len, const {element_type}* values) {{\n\
             \x20 {array_type} result = {{ len, NULL }};\n\
             \x20 if (len == 0) return result;\n\
             \x20 if (len > SIZE_MAX / sizeof({element_type})) {{ fprintf(stderr, \"array allocation is too large\\n\"); exit(1); }}\n\
             \x20 result.data = ({element_type}*)malloc(len * sizeof({element_type}));\n\
             \x20 if (!result.data) {{ fprintf(stderr, \"array allocation failed\\n\"); exit(1); }}\n\
             \x20 memcpy(result.data, values, len * sizeof({element_type}));\n\
             \x20 return result;\n\
             }}\n\
             static {array_type} ku_array_clone_{suffix}({array_type} array) {{\n\
             \x20 {array_type} result = ku_array_make_{suffix}(array.len, array.data);\n\
             \x20 for (size_t index = 0; index < result.len; index++) result.data[index] = {clone_element};\n\
             \x20 return result;\n\
             }}\n\
             static {array_type} ku_array_move_{suffix}({array_type}* array) {{\n\
             \x20 {array_type} result = *array;\n\
             \x20 array->len = 0;\n\
             \x20 array->data = NULL;\n\
             \x20 return result;\n\
             }}\n\
             static void ku_array_drop_{suffix}({array_type}* array) {{\n\
             \x20 if (!array || !array->data) return;\n\
             \x20 for (size_t index = 0; index < array->len; index++) {{ {drop_element} }}\n\
             \x20 free(array->data);\n\
             \x20 array->data = NULL;\n\
             \x20 array->len = 0;\n\
             }}\n\
             static {element_type} ku_array_get_{suffix}({array_type} array, int64_t index) {{\n\
             \x20 if (index < 0 || (uint64_t)index >= array.len) ku_array_bounds_fail(index, array.len);\n\
             \x20 return array.data[index];\n\
             }}\n\
             static {element_type}* ku_array_at_{suffix}({array_type}* array, int64_t index) {{\n\
             \x20 if (index < 0 || (uint64_t)index >= array->len) ku_array_bounds_fail(index, array->len);\n\
             \x20 return &array->data[index];\n\
             }}\n\
             static int64_t ku_array_len_{suffix}({array_type} array) {{ return (int64_t)array.len; }}\n\
             static bool ku_array_is_empty_{suffix}({array_type} array) {{ return array.len == 0; }}\n\
             static {array_type} ku_array_push_{suffix}({array_type} array, {element_type} value) {{\n\
             \x20 size_t len = array.len + 1;\n\
             \x20 if (len > SIZE_MAX / sizeof({element_type})) {{ fprintf(stderr, \"array allocation is too large\\n\"); exit(1); }}\n\
             \x20 {element_type}* data = ({element_type}*)malloc(len * sizeof({element_type}));\n\
             \x20 if (!data) {{ fprintf(stderr, \"array allocation failed\\n\"); exit(1); }}\n\
             \x20 for (size_t index = 0; index < array.len; index++) data[index] = {clone_element};\n\
             \x20 data[array.len] = {clone_pushed};\n\
             \x20 return ({array_type}){{ len, data }};\n\
             }}\n\n"
    ));
    Ok(())
}

fn collect_array_element_type(ty: &IrType, output: &mut Vec<IrType>) {
    match ty {
        IrType::Array(inner) => {
            collect_array_element_type(inner, output);
            if !output.contains(inner.as_ref()) {
                output.push(*inner.clone());
            }
        }
        IrType::Result(inner) => collect_array_element_type(inner, output),
        _ => {}
    }
}

fn collect_array_expr_types(expr: &IrExpr, output: &mut Vec<IrType>) {
    collect_array_element_type(&expr.ty, output);
    match &expr.kind {
        IrExprKind::Unary { expr, .. } | IrExprKind::TryUnwrap(expr) => {
            collect_array_expr_types(expr, output)
        }
        IrExprKind::Binary { left, right, .. } => {
            collect_array_expr_types(left, output);
            collect_array_expr_types(right, output);
        }
        IrExprKind::Call { callee, args, .. } => {
            collect_array_expr_types(callee, output);
            for arg in args {
                collect_array_expr_types(arg, output);
            }
        }
        IrExprKind::Array(values) => {
            for value in values {
                collect_array_expr_types(value, output);
            }
        }
        IrExprKind::Index { target, index } => {
            collect_array_expr_types(target, output);
            collect_array_expr_types(index, output);
        }
        IrExprKind::Field { target, .. } => collect_array_expr_types(target, output),
        IrExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_array_expr_types(value, output);
            }
        }
        IrExprKind::CellLoad(inner) => collect_array_expr_types(inner, output),
        IrExprKind::Literal(_)
        | IrExprKind::Local(_)
        | IrExprKind::Temp(_)
        | IrExprKind::MakeClosure { .. }
        | IrExprKind::CapturedCell(_) => {}
    }
}

fn collect_array_lvalue_types(target: &IrLValue, output: &mut Vec<IrType>) {
    match target {
        IrLValue::Local(_) => {}
        IrLValue::Index { target, index } => {
            collect_array_expr_types(target, output);
            collect_array_expr_types(index, output);
        }
        IrLValue::Field { target, .. } => collect_array_expr_types(target, output),
    }
}

fn emit_function(out: &mut String, function: &IrFunction) -> KuResult<()> {
    let owned_locals = collect_owned_locals(function);
    out.push_str(&format!(
        "{} {}(",
        c_type(&function.return_type)?,
        c_symbol(&function.name)
    ));
    // A lifted closure body carries a leading `void* __env` (Stage 6a env is
    // always NULL); the leading comma below then separates real parameters.
    if function.is_closure_body {
        out.push_str("void* __env");
    }
    for (index, param) in function.params.iter().enumerate() {
        if index > 0 || function.is_closure_body {
            out.push_str(", ");
        }
        out.push_str(&format!("{} {}", c_type(&param.ty)?, c_ident(&param.name)));
    }
    out.push_str(") {\n");
    out.push_str("  if (++__ku_call_depth > KU_MAX_CALL_DEPTH) { fprintf(stderr, \"maximum call depth exceeded: %d\\n\", KU_MAX_CALL_DEPTH); exit(1); }\n");
    if function.is_closure_body {
        if function.captures.is_empty() {
            out.push_str("  (void)__env;\n");
        } else {
            // Stage 6b: recover the typed env holding the captured cell pointers.
            out.push_str(&format!(
                "  KuEnv_{id}* __e = (KuEnv_{id}*)__env;\n",
                id = function.id.0
            ));
        }
    }
    for local in &owned_locals {
        if local.is_param {
            continue;
        }
        out.push_str(&format!(
            "  {} {} = {};\n",
            c_type(&local.ty)?,
            local.name,
            c_zero_initializer(&local.ty)?
        ));
    }
    for block in &function.blocks {
        emit_block(out, block, &function.return_type, &owned_locals)?;
    }
    if function.return_type == IrType::Void {
        emit_owned_cleanup(out, &owned_locals)?;
        out.push_str("  return;\n");
    }
    out.push_str("}\n");
    Ok(())
}

fn emit_block(
    out: &mut String,
    block: &IrBlock,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    if block.id.0 != 0 {
        out.push_str(&format!("block{}:;\n", block.id.0));
    }
    for inst in &block.instructions {
        emit_inst(out, inst, return_type, owned_locals)?;
    }
    if block.terminator == IrTerminator::Unreachable
        && matches!(
            block.instructions.last(),
            Some(IrInst::Fail(_) | IrInst::Panic(_))
        )
    {
        return Ok(());
    }
    emit_terminator(out, &block.terminator, return_type, owned_locals)
}

fn emit_inst(
    out: &mut String,
    inst: &IrInst,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    match inst {
        IrInst::Temp { id, ty, value } => {
            if try_emit_object_construction(out, &format!("t{}", id.0), value)? {
                return Ok(());
            }
            if emit_statement_intrinsic(out, value)? {
                out.push_str(&format!(
                    "  {} t{} = {};\n",
                    c_type(ty)?,
                    id.0,
                    c_zero_value(ty)?
                ));
                return Ok(());
            }
            if is_c_owned_type(ty) {
                // A shallow container-alias temp (array/object index read) is not
                // an owner, so it is neither dropped at cleanup nor here. An owned
                // temp (including a struct-field move) IS the owner: drop its prior
                // value before overwriting, or a temp reused across loop iterations
                // leaks the previous value (at first use the temp is zero-init, so
                // the drop is a no-op).
                let borrowed = matches!(
                    value.kind,
                    IrExprKind::Field { .. } | IrExprKind::Index { .. }
                ) && c_move_place(value).ok().flatten().is_none();
                if borrowed {
                    out.push_str(&format!("  t{} = {};\n", id.0, c_value_expr(value)?));
                } else {
                    out.push_str(&format!(
                        "  {{ {} __ku_store = {};\n",
                        c_type(ty)?,
                        c_value_expr(value)?
                    ));
                    emit_drop_expr(out, ty, &format!("t{}", id.0))?;
                    out.push_str(&format!("  t{} = __ku_store; }}\n", id.0));
                }
            } else {
                out.push_str(&format!(
                    "  {} t{} = {};\n",
                    c_type(ty)?,
                    id.0,
                    c_expr(value)?
                ));
            }
        }
        IrInst::BindOk { id, ty, result } => {
            if is_c_owned_type(ty) {
                // The temp is hoisted (owned), so drop its previous value before
                // overwriting: when the unwrapped value is only borrowed afterward
                // (e.g. `pg.rows(pg.query(..)?)`), the temp is reused across a loop
                // and a bare assignment would leak the prior iteration's handle.
                // Taking from `result` does not touch this temp, so materializing the
                // new value before dropping the old is safe (zero-init → no-op first).
                out.push_str(&format!(
                    "  {{ {} __ku_store = ku_result_take_{}(&{});\n",
                    c_type(ty)?,
                    c_type_suffix(ty)?,
                    c_addressable_expr(result)?
                ));
                emit_drop_expr(out, ty, &format!("t{}", id.0))?;
                out.push_str(&format!("  t{} = __ku_store; }}\n", id.0));
            } else {
                out.push_str(&format!(
                    "  {} t{} = ku_result_take_{}(&{});\n",
                    c_type(ty)?,
                    id.0,
                    c_type_suffix(ty)?,
                    c_addressable_expr(result)?
                ));
            }
        }
        IrInst::Let { name, ty, value } => {
            if is_c_owned_type(ty) {
                // The owned local is hoisted and zero-initialized at the function
                // head, so this is an assignment. Drop the previous value first: a
                // `let`-style binding inside a loop body re-executes every
                // iteration, and without the drop each iteration's owned value
                // would leak (at the first iteration the local is {0}, so the drop
                // is a no-op).
                let materialized = if is_native_zero(value) {
                    c_zero_initializer(ty)?
                } else {
                    c_value_expr(value)?
                };
                out.push_str(&format!("  {{ {} __ku_store = {};\n", c_type(ty)?, materialized));
                emit_drop_expr(out, ty, &c_ident(name))?;
                out.push_str(&format!("  {} = __ku_store; }}\n", c_ident(name)));
            } else {
                out.push_str(&format!(
                    "  {} {} = {};\n",
                    c_type(ty)?,
                    c_ident(name),
                    if is_native_zero(value) {
                        c_zero_initializer(ty)?
                    } else {
                        c_value_expr(value)?
                    }
                ));
            }
        }
        IrInst::Store { target, value } => {
            if let IrLValue::Local(name) = target {
                if let Some(local) = owned_locals.iter().find(|local| local.source_name == *name) {
                    out.push_str(&format!(
                        "  {{ {} __ku_store = {};\n",
                        c_type(&local.ty)?,
                        c_value_expr(value)?
                    ));
                    emit_drop_expr(out, &local.ty, &local.name)?;
                    out.push_str(&format!("  {} = __ku_store; }}\n", local.name));
                    return Ok(());
                }
            }
            // Overwriting an owned struct field must free the value it held first,
            // or that heap buffer is leaked. The new value is materialized before
            // the drop in case it reads the old field. (A field that was already
            // moved out is cleared, so its drop is a harmless no-op.)
            if matches!(target, IrLValue::Field { .. }) && is_c_owned_type(&value.ty) {
                let lvalue = c_lvalue(target)?;
                out.push_str(&format!(
                    "  {{ {} __ku_store = {};\n",
                    c_type(&value.ty)?,
                    c_value_expr(value)?
                ));
                emit_drop_expr(out, &value.ty, &lvalue)?;
                out.push_str(&format!("  {lvalue} = __ku_store; }}\n"));
                return Ok(());
            }
            out.push_str(&format!(
                "  {} = {};\n",
                c_lvalue(target)?,
                c_value_expr(value)?
            ));
        }
        IrInst::Print(value) => emit_print(out, value)?,
        IrInst::Expr(value) => emit_expr_statement(out, value)?,
        IrInst::Fail(value) => {
            let IrType::Result(inner) = return_type else {
                return Err(unsupported("native C fail requires a Result return type"));
            };
            let result = format!(
                "({}){{ false, {}, {} }}",
                c_type(return_type)?,
                c_zero_value(inner)?,
                c_error_expr(value)?
            );
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str("  __ku_call_depth--;\n");
            out.push_str(&format!("  return {result};\n"));
        }
        IrInst::Panic(value) => {
            if value.ty == IrType::Str {
                out.push_str(&format!(
                    "  fprintf(stderr, \"%.*s\\n\", (int)({}).len, (const char*)({}).ptr); exit(1);\n",
                    c_expr(value)?,
                    c_expr(value)?
                ));
            } else {
                out.push_str("  fprintf(stderr, \"panic\\n\"); exit(1);\n");
            }
        }
        IrInst::BeginTry { .. } | IrInst::EndTry => {}
        IrInst::BindError { name, result } => {
            // The catch binding is declared as an owned local at the function
            // head. Move the error out of the slot so the slot's own cleanup
            // won't free it from under the binding (and the binding owns its
            // KuStrings, dropped at scope exit). The slot now holds a bare
            // KuError (older code read `.error` off a Result-typed slot).
            let is_error = matches!(&result.ty, IrType::Named(n) if n == "__ku_error_type");
            let source = if is_error {
                format!("ku_error_move(&{})", c_expr(result)?)
            } else {
                format!("ku_error_move(&({}).error)", c_expr(result)?)
            };
            // Drop the binding's previous value before overwriting: in a loop the
            // catch binding is reused each iteration, so without this the prior
            // error's owned strings leak.
            let ident = c_ident(name);
            out.push_str(&format!(
                "  {{ KuError __ku_store = {source};\n  ku_error_drop(&{ident});\n  {ident} = __ku_store; }}\n"
            ));
        }

        IrInst::CellNew { name, ty, init } => {
            // Stage 6b: box a captured local. The pointer is pre-declared NULL at
            // the function head, and the owned-local cleanup releases the final
            // value on every exit path. A binding re-boxed each loop iteration is
            // CellNew'd repeatedly, so release the previous cell before overwriting
            // it or every prior iteration's box leaks. The new cell is built before
            // the release (like drop-then-store), and `release(NULL)` on the first
            // pass is a no-op.
            let suffix = c_type_suffix(ty)?;
            out.push_str(&format!(
                "  {{ KuCell_{suffix}* __ku_cell = ku_cell_{suffix}_new({}); ku_cell_{suffix}_release({name}); {name} = __ku_cell; }}\n",
                c_value_expr(init)?,
                name = c_ident(name),
            ));
        }
        IrInst::CellStore { cell, value } => {
            let inner = match &cell.ty {
                IrType::Cell(inner) => Some((**inner).clone()),
                _ => None,
            };
            // Stage 6c: reassigning a captured/boxed owned payload drops the cell's
            // old value and moves the new value in (rule 4). The new value is
            // evaluated into a temp BEFORE the old value is dropped, so an RHS that
            // reads the same cell (e.g. `s = s + x`, `xs = f(xs)`) still sees the
            // old value (no self-read UAF). Copy payloads assign directly.
            match inner {
                Some(IrType::Str) => {
                    out.push_str(&format!(
                        "  {{ KuString __ku_cell_new = {}; ku_string_drop(&({})->value); ({})->value = __ku_cell_new; }}\n",
                        c_value_expr(value)?,
                        c_expr(cell)?,
                        c_expr(cell)?
                    ));
                }
                Some(IrType::Array(element)) => {
                    let payload = c_array_type(&element)?;
                    out.push_str(&format!(
                        "  {{ {payload} __ku_cell_new = {}; ku_array_drop_{}(&({})->value); ({})->value = __ku_cell_new; }}\n",
                        c_value_expr(value)?,
                        c_type_suffix(&element)?,
                        c_expr(cell)?,
                        c_expr(cell)?
                    ));
                }
                Some(IrType::Named(name)) if name == "__ku_object" => {
                    out.push_str(&format!(
                        "  {{ KuObject* __ku_cell_new = {}; ku_object_drop(({})->value); ({})->value = __ku_cell_new; }}\n",
                        c_value_expr(value)?,
                        c_expr(cell)?,
                        c_expr(cell)?
                    ));
                }
                _ => {
                    out.push_str(&format!(
                        "  ({})->value = {};\n",
                        c_expr(cell)?,
                        c_value_expr(value)?
                    ));
                }
            }
        }
        IrInst::CellRelease(_) => {
            // Boxed locals are released through the owned-local cleanup (so early
            // returns release too); no standalone release is emitted here.
        }
        IrInst::DefineClosure { .. } | IrInst::Unsupported { .. } => {
            return Err(unsupported(format!(
                "native C prototype cannot lower IR instruction '{inst}'"
            )));
        }
    }
    Ok(())
}

fn emit_expr_statement(out: &mut String, value: &IrExpr) -> KuResult<()> {
    if emit_statement_intrinsic(out, value)? {
        return Ok(());
    }
    out.push_str(&format!("  (void){};\n", c_expr(value)?));
    Ok(())
}

fn emit_statement_intrinsic(out: &mut String, value: &IrExpr) -> KuResult<bool> {
    let IrExprKind::Call { args, kind, .. } = &value.kind else {
        return Ok(false);
    };
    let IrCallKind::Intrinsic(name) = kind else {
        return Ok(false);
    };
    match name.as_str() {
        "println" => {
            let value = args
                .first()
                .ok_or_else(|| unsupported("println requires one argument"))?;
            emit_print(out, value)?;
            out.push_str("  printf(\"\\n\");\n  fflush(stdout);\n");
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn emit_print(out: &mut String, value: &IrExpr) -> KuResult<()> {
    match value.ty {
        IrType::Int => {
            out.push_str(&format!(
                "  printf(\"%lld\", (long long){});\n  fflush(stdout);\n",
                c_expr(value)?
            ));
        }
        // Booleans print as `true`/`false` to match the interpreter, not the
        // numeric `1`/`0` that `%lld` would produce.
        IrType::Bool => {
            out.push_str(&format!(
                "  printf(\"%s\", ({}) ? \"true\" : \"false\");\n  fflush(stdout);\n",
                c_expr(value)?
            ));
        }
        IrType::Str => {
            // Evaluate the argument ONCE: a second evaluation would re-run any side
            // effect (e.g. a move-and-clear projection) and read a cleared string.
            out.push_str(&format!(
                "  {{ KuString __ku_p = {}; printf(\"%.*s\", (int)__ku_p.len, (const char*)__ku_p.ptr); }}\n  fflush(stdout);\n",
                c_expr(value)?
            ));
        }
        IrType::Named(ref name) if name == "__ku_value" => {
            out.push_str(&format!("  ku_value_print({});\n  fflush(stdout);\n", c_expr(value)?));
        }
        _ => {
            return Err(unsupported(
                "native C prototype print supports int/bool/str/KuValue",
            ))
        }
    }
    Ok(())
}

fn emit_terminator(
    out: &mut String,
    terminator: &IrTerminator,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    match terminator {
        IrTerminator::Next => Ok(()),
        IrTerminator::Jump(target) => {
            out.push_str(&format!("  goto block{};\n", target.0));
            Ok(())
        }
        IrTerminator::Branch {
            condition,
            then_block,
            else_block,
        } => {
            out.push_str(&format!(
                "  if ({}) goto block{}; else goto block{};\n",
                c_expr(condition)?,
                then_block.0,
                else_block.0
            ));
            Ok(())
        }
        IrTerminator::ForEach { .. } => Err(unsupported(
            "native C prototype does not support for lowering yet",
        )),
        IrTerminator::ResultBranch {
            result,
            ok_block,
            err_block,
        } => {
            out.push_str(&format!(
                "  if ({}.ok) goto block{}; else goto block{};\n",
                c_expr(result)?,
                ok_block.0,
                err_block.0
            ));
            Ok(())
        }
        IrTerminator::JumpErr { result, target } => {
            out.push_str(&format!(
                "  (void){}; goto block{};\n",
                c_expr(result)?,
                target.0
            ));
            Ok(())
        }
        IrTerminator::PropagateErr(value) => {
            let IrType::Result(return_inner) = return_type else {
                return Err(unsupported(
                    "native C prototype can only propagate errors from Result functions",
                ));
            };
            // `value` is a failed Result (take its `.error`) or a bare KuError
            // re-propagated out of a finally block (move it directly).
            let is_error = matches!(&value.ty, IrType::Named(n) if n == "__ku_error_type");
            if !is_error && !matches!(value.ty, IrType::Result(_)) {
                return Err(unsupported(
                    "native C error propagation requires a Result or error value",
                ));
            }
            let expr = c_expr(value)?;
            let take_error = if is_error {
                format!("ku_error_move(&{expr})")
            } else {
                format!("ku_error_move(&{expr}.error)")
            };
            out.push_str(&format!("  {{ KuError __ku_error = {take_error};\n"));
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str("  __ku_call_depth--;\n");
            out.push_str(&format!(
                "  return ({}){{ false, {}, __ku_error }}; }}\n",
                c_type(return_type)?,
                c_zero_value(return_inner)?
            ));
            Ok(())
        }
        IrTerminator::Return(Some(value)) => {
            if is_c_owned_type(&value.ty) {
                out.push_str(&format!(
                    "  {{ {} __ku_return = {};\n",
                    c_type(&value.ty)?,
                    c_value_expr(value)?
                ));
                emit_owned_cleanup(out, owned_locals)?;
                out.push_str("  __ku_call_depth--;\n");
                out.push_str("  return __ku_return; }\n");
            } else {
                let value = c_value_expr(value)?;
                emit_owned_cleanup(out, owned_locals)?;
                out.push_str("  __ku_call_depth--;\n");
                out.push_str(&format!("  return {value};\n"));
            }
            Ok(())
        }
        IrTerminator::Return(None) => {
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str("  __ku_call_depth--;\n");
            out.push_str("  return;\n");
            Ok(())
        }
        IrTerminator::Unreachable => {
            out.push_str("  abort();\n");
            Ok(())
        }
    }
}

fn c_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.kind {
        IrExprKind::Literal(value) => {
            if value == "<native-zero>" {
                c_zero_initializer(&expr.ty)
            } else if expr.ty == IrType::Null && value == "null" {
                Ok("0".to_string())
            } else if expr.ty == IrType::Str {
                c_str_literal_static(value)
            } else {
                Ok(value.clone())
            }
        }
        IrExprKind::Local(name) => Ok(c_symbol(name)),
        IrExprKind::Temp(id) => Ok(format!("t{}", id.0)),
        IrExprKind::StructLiteral { name, fields } => {
            // The struct TAKES OWNERSHIP of each field value, so move it in
            // (clearing the source) rather than shallow-copying: `c_expr` would
            // leave an owned source local/temp still owning the same buffer, which
            // the struct's field and the source would then both free.
            let fields = fields
                .iter()
                .map(|(field, value)| Ok(format!(".{} = {}", c_ident(field), c_value_expr(value)?)))
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            Ok(format!("({}){{ {fields} }}", c_struct_type(name)))
        }
        IrExprKind::Unary { op, expr } => Ok(format!("({}{})", c_unary(*op), c_expr(expr)?)),
        IrExprKind::Binary { left, op, right }
            if left.ty == IrType::Str
                && right.ty == IrType::Str
                && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            Ok(format!(
                "(ku_string_equal({}, {}) {} true)",
                c_expr(left)?,
                c_expr(right)?,
                if *op == BinaryOp::Equal { "==" } else { "!=" }
            ))
        }
        IrExprKind::Binary { left, op, right }
            if left.ty == IrType::Str && right.ty == IrType::Str && *op == BinaryOp::Add =>
        {
            Ok(format!("ku_string_concat({}, {})", c_expr(left)?, c_expr(right)?))
        }
        IrExprKind::Binary { left, op, .. }
            if matches!(left.ty, IrType::Array(_))
                && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            Err(unsupported(
                "native C prototype does not support array equality yet",
            ))
        }
        IrExprKind::Binary { left, op, .. }
            if matches!(
                &left.ty,
                IrType::Named(name) if enum_type_name(name).is_some()
            ) && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            Err(unsupported(
                "native C prototype does not support enum equality yet",
            ))
        }
        IrExprKind::Binary { left, op, right } => Ok(format!(
            "({} {} {})",
            c_expr(left)?,
            c_binary(*op),
            c_expr(right)?
        )),
        IrExprKind::Call { callee, args, kind } => {
            if let IrCallKind::Intrinsic(name) = kind {
                return c_intrinsic_expr(name, args, &expr.ty);
            }
            if let IrCallKind::Indirect = kind {
                // Closure call: `cl.invoke(cl.env, args...)`. The callee is a
                // side-effect-free Local/Temp closure value, so evaluating it
                // twice (once for invoke, once for env) is safe (Stage 6a).
                let callee = c_expr(callee)?;
                let mut parts = vec![format!("({callee}).env")];
                for arg in args {
                    parts.push(c_arg_value_expr(arg)?);
                }
                return Ok(format!("({callee}).invoke({})", parts.join(", ")));
            }
            if !matches!(kind, IrCallKind::Direct(_)) {
                return Err(unsupported(
                    "native C prototype only supports direct function calls",
                ));
            }
            let callee = c_expr(callee)?;
            let args = args
                .iter()
                .map(c_arg_value_expr)
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            Ok(format!("{callee}({args})"))
        }
        IrExprKind::Field { target, name } => {
            Ok(format!("({}).{}", c_expr(target)?, c_ident(name)))
        }
        IrExprKind::Array(values) => {
            let IrType::Array(element) = &expr.ty else {
                return Err(unsupported(
                    "native C array literal is missing its element type",
                ));
            };
            if values.is_empty() {
                return Ok(format!(
                    "ku_array_make_{}(0, NULL)",
                    c_type_suffix(element)?
                ));
            }
            let len = values.len();
            let values = values
                .iter()
                .map(c_value_expr)
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            Ok(format!(
                "ku_array_make_{}({}, ({}[]){{ {} }})",
                c_type_suffix(element)?,
                len,
                c_type(element)?,
                values
            ))
        }
        IrExprKind::Index { target, index } => {
            let IrType::Array(element) = &target.ty else {
                return Err(unsupported(
                    "native C index expression requires an array target",
                ));
            };
            Ok(format!(
                "ku_array_get_{}({}, {})",
                c_type_suffix(element)?,
                c_expr(target)?,
                c_expr(index)?
            ))
        }
        IrExprKind::MakeClosure {
            function_id,
            captures,
        } => {
            let suffix = c_type_suffix(&expr.ty)?;
            let invoke = closure_invoke_symbol(*function_id).ok_or_else(|| {
                unsupported(format!(
                    "native C closure references unknown function #{}",
                    function_id.0
                ))
            })?;
            if captures.is_empty() {
                // No captures: env is NULL (Stage 6a).
                Ok(format!("(KuClosure_{suffix}){{ {invoke}, NULL }}"))
            } else {
                // Stage 6b: allocate an env, passing the outer cell pointers (the
                // env retains each). The env id is the closure body's function id.
                let cells = captures
                    .iter()
                    .map(|(name, _)| c_symbol(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!(
                    "(KuClosure_{suffix}){{ {invoke}, ku_env_{}_new({cells}) }}",
                    function_id.0
                ))
            }
        }
        IrExprKind::CellLoad(inner) => Ok(format!("({})->value", c_expr(inner)?)),
        IrExprKind::CapturedCell(name) => Ok(format!("__e->{}", c_ident(name))),
        IrExprKind::TryUnwrap(_) => Err(unsupported(format!(
            "native C prototype cannot lower expression '{expr}'"
        ))),
    }
}

/// The C expression for a *place* an owned value can be moved out of, and whose
/// source must therefore be cleared: a local, a temp, or a field of a struct held
/// in one.
///
/// Reading a field in value position is a MOVE of that field. Without clearing the
/// source, the owning struct's own later drop frees the very buffer the moved value
/// now owns. That is a real double-free, not a leak: `http.text(req.body)` handed
/// the response a `KuString` copied out of `req`, and the closure's trailing
/// `ku_drop_struct___ku_http_request(&req)` then freed those bytes, so the response
/// was written from freed memory and dropping it corrupted the heap.
///
/// Only struct fields qualify. Dynamic-object (`__ku_object`) and `__ku_value`
/// field reads lower to their own accessors rather than a C field, an enum is not
/// a field target, and a field of a non-place (a call result) has no other owner
/// left to clear.
fn c_move_place(expr: &IrExpr) -> KuResult<Option<String>> {
    match &expr.kind {
        IrExprKind::Local(name) => Ok(Some(c_symbol(name))),
        IrExprKind::Temp(id) => Ok(Some(format!("t{}", id.0))),
        IrExprKind::Field { target, name } => {
            let IrType::Named(type_name) = &target.ty else {
                return Ok(None);
            };
            if type_name == "__ku_object"
                || type_name == "__ku_value"
                || type_name == "__ku_http_server"
                || enum_type_name(type_name).is_some()
            {
                return Ok(None);
            }
            Ok(c_move_place(target)?.map(|place| format!("({}).{}", place, c_ident(name))))
        }
        // A closure-captured local lives in a refcounted cell, and `(cell)->value`
        // is an lvalue: moving out of it clears the payload so the cell's release
        // drops a zeroed value instead of freeing the buffer a second time.
        IrExprKind::CellLoad(inner) => Ok(Some(format!("({})->value", c_expr(inner)?))),
        _ => Ok(None),
    }
}

fn c_value_expr(expr: &IrExpr) -> KuResult<String> {
    // Stage 8a: the server value is a shared heap pointer; assigning or passing it
    // is a plain copy (no move/clone helper), so it never needs a `ku_move_*`.
    if matches!(&expr.ty, IrType::Named(name) if name == "__ku_http_server") {
        return c_expr(expr);
    }
    if let IrExprKind::Call { kind, args, .. } = &expr.kind {
        if matches!(kind, IrCallKind::Intrinsic(name) if name == "__ku_clone") {
            let value = args
                .first()
                .ok_or_else(|| unsupported("clone intrinsic requires one argument"))?;
            return c_clone_expr(value);
        }
    }
    if let IrType::Array(element) = &expr.ty {
        if let Some(place) = c_move_place(expr)? {
            return Ok(format!(
                "ku_array_move_{}(&{})",
                c_type_suffix(element)?,
                place
            ));
        }
    }
    if expr.ty == IrType::Str {
        if let Some(place) = c_move_place(expr)? {
            return Ok(format!("ku_string_move(&{place})"));
        }
    }
    if let IrType::Result(inner) = &expr.ty {
        if let Some(place) = c_move_place(expr)? {
            return Ok(format!(
                "ku_result_move_{}(&{})",
                c_type_suffix(inner)?,
                place
            ));
        }
    }
    if let IrType::Closure { .. } = &expr.ty {
        // Stage 6b: closures are single-owner; assigning/passing one moves it
        // (transfers the env and nulls the source), so the env is released
        // exactly once. Env-less (Stage 6a) closures move harmlessly.
        let suffix = c_type_suffix(&expr.ty)?;
        if let Some(place) = c_move_place(expr)? {
            return Ok(format!("ku_closure_move_{}(&{})", suffix, place));
        }
    }
    if let IrType::Named(name) = &expr.ty {
        // Moving a KuError out of a Result's `.error` field (the try error-slot
        // store): clear the source field so the Result's own later cleanup can't
        // double-free the strings the slot now owns. This one is not a struct
        // place -- the target is a Result -- so it needs its own move.
        if name == "__ku_error_type" {
            if let IrExprKind::Field { target, name: field } = &expr.kind {
                return Ok(format!("ku_error_move(&({}).{})", c_expr(target)?, field));
            }
        }
        if let Some(place) = c_move_place(expr)? {
            return Ok(if name == "__ku_object" {
                format!("ku_object_move(&{place})")
            } else if name == "__ku_value" {
                format!("ku_value_move(&{place})")
            } else {
                format!("{}(&{})", c_named_move_function(name), place)
            });
        }
    }
    c_expr(expr)
}

/// Lower a call argument. Identical to [`c_value_expr`] except for how a function
/// value is handed to the callee (Stage 6d, matching the interpreter's shared
/// ownership):
///   * A **named** closure binding (`Local`) is passed by *retain* — the callee
///     receives its own ref-counted reference (`ku_closure_clone`, env rc++),
///     while the caller keeps its binding usable for later calls and releases its
///     own reference at scope end. The callee owns and releases its copy. This
///     stays sound even when the callee returns/stores the argument.
///   * A **temporary** closure (`Temp`, e.g. a fresh `(x) => ...` literal) has no
///     other owner, so it is moved (transferred) via `c_value_expr`.
/// Every non-closure type keeps `c_value_expr`'s move/clone semantics.
fn c_arg_value_expr(expr: &IrExpr) -> KuResult<String> {
    if let IrType::Closure { params, ret } = &expr.ty {
        if let IrExprKind::Local(name) = &expr.kind {
            return Ok(format!(
                "ku_closure_clone_{}({})",
                closure_signature_suffix(params, ret)?,
                c_symbol(name)
            ));
        }
    }
    c_value_expr(expr)
}

fn c_clone_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.ty {
        IrType::Array(element) => Ok(format!(
            "ku_array_clone_{}({})",
            c_type_suffix(element)?,
            c_expr(expr)?
        )),
        IrType::Str => Ok(format!("ku_string_clone({})", c_expr(expr)?)),
        IrType::Result(inner) => Ok(format!(
            "ku_result_clone_{}({})",
            c_type_suffix(inner)?,
            c_expr(expr)?
        )),
        IrType::Named(name) if name == "__ku_object" => {
            Ok(format!("ku_object_clone({})", c_expr(expr)?))
        }
        IrType::Named(name) if name == "__ku_value" => {
            Ok(format!("ku_value_clone({})", c_expr(expr)?))
        }
        IrType::Closure { .. } => Ok(format!(
            "ku_closure_clone_{}({})",
            c_type_suffix(&expr.ty)?,
            c_expr(expr)?
        )),
        IrType::Named(name) => Ok(format!(
            "{}({})",
            c_named_clone_function(name),
            c_expr(expr)?
        )),
        _ => Err(unsupported(format!(
            "native C clone() is not implemented for {} yet",
            expr.ty
        ))),
    }
}

fn c_addressable_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.kind {
        IrExprKind::Local(name) => Ok(c_symbol(name)),
        IrExprKind::Temp(id) => Ok(format!("t{}", id.0)),
        _ => Err(unsupported(
            "native C Result unwrapping requires a materialized local result",
        )),
    }
}

fn c_error_expr(value: &IrExpr) -> KuResult<String> {
    if value.ty == IrType::Named("__ku_error_type".to_string()) {
        // Move an existing Error value (its KuStrings are owned).
        match &value.kind {
            IrExprKind::Local(name) => Ok(format!("ku_error_move(&{})", c_symbol(name))),
            IrExprKind::Temp(id) => Ok(format!("ku_error_move(&t{})", id.0)),
            // A freshly-built error (e.g. `__ku_error_make(...)`) — already an rvalue.
            _ => c_expr(value),
        }
    } else if value.ty == IrType::Str {
        // Move the message string into the error.
        Ok(format!("ku_error_message({})", c_value_expr(value)?))
    } else {
        Err(unsupported(
            "native C errors currently require a string or Error value",
        ))
    }
}

fn c_lvalue(target: &IrLValue) -> KuResult<String> {
    match target {
        IrLValue::Local(name) => Ok(c_ident(name)),
        IrLValue::Field { target, name } => {
            // Field stores through a heap pointer (`KuHttpServer*` for the native
            // HTTP server, `KuObject*` for a dynamic object) must dereference with
            // `->`; struct/enum values are held inline and use `.`. Deciding from
            // the emitted C type rather than from a hard-coded type name keeps this
            // right for every pointer-backed type instead of just the one that
            // first needed it (`app.max_body_bytes = 4`).
            let op = match c_type(&target.ty) {
                Ok(ty) if ty.ends_with('*') => "->",
                _ => ".",
            };
            Ok(format!("({}){}{}", c_expr(target)?, op, c_ident(name)))
        }
        IrLValue::Index { target, index } => {
            let IrType::Array(element) = &target.ty else {
                return Err(unsupported(
                    "native C index assignment requires an array target",
                ));
            };
            Ok(format!(
                "*ku_array_at_{}(&({}), {})",
                c_type_suffix(element)?,
                c_expr(target)?,
                c_expr(index)?
            ))
        }
    }
}

fn c_type(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int => Ok("int64_t".to_string()),
        IrType::Bool => Ok("bool".to_string()),
        IrType::Str => Ok("KuString".to_string()),
        IrType::Null => Ok("uint8_t".to_string()),
        IrType::Array(inner) => c_array_type(inner),
        IrType::Result(inner) => c_result_type(inner),
        IrType::Named(name) if name == "__ku_error_type" => Ok("KuError".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok("KuObject*".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("KuValue".to_string()),
        // Stage 8a: the native HTTP server is a heap pointer shared between route
        // registration and the accept loop (never copied by value).
        IrType::Named(name) if name == "__ku_http_server" => Ok("KuHttpServer*".to_string()),
        // pg opaque handles are the raw libpq pointers; NULL means moved-out/closed.
        IrType::Named(name) if name == "__ku_pg_conn" => Ok("PGconn*".to_string()),
        IrType::Named(name) if name == "__ku_pg_result" => Ok("PGresult*".to_string()),
        IrType::Named(name) if name == "__ku_pg_pool" => Ok("KuPgPool*".to_string()),
        IrType::Named(name) if name == "__ku_redis_conn" => Ok("KuRedis*".to_string()),
        IrType::Named(name) if name == "__ku_mysql_conn" => Ok("MYSQL*".to_string()),
        IrType::Named(name) if name == "__ku_mysql_result" => Ok("MYSQL_RES*".to_string()),
        IrType::Named(name) => Ok(match enum_type_name(name) {
            Some(name) => c_enum_type(name),
            None => c_struct_type(name),
        }),
        IrType::Closure { params, ret } => {
            Ok(format!("KuClosure_{}", closure_signature_suffix(params, ret)?))
        }
        IrType::Cell(inner) => Ok(format!("KuCell_{}*", c_type_suffix(inner)?)),
        IrType::Void => Ok("void".to_string()),
        _ => Err(unsupported(format!(
            "native C prototype does not support type {ty}"
        ))),
    }
}

fn emit_result_abi(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut result_types = Vec::new();
    for function in &program.functions {
        collect_result_type(&function.return_type, &mut result_types)?;
        for param in &function.params {
            collect_result_type(&param.ty, &mut result_types)?;
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { ty, value, .. } => {
                        collect_result_type(ty, &mut result_types)?;
                        collect_result_type(&value.ty, &mut result_types)?;
                    }
                    IrInst::BindOk { result, .. } => {
                        collect_result_type(&result.ty, &mut result_types)?;
                    }
                    IrInst::Let { ty, value, .. } => {
                        collect_result_type(ty, &mut result_types)?;
                        collect_result_type(&value.ty, &mut result_types)?;
                    }
                    IrInst::Store { value, .. }
                    | IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value) => {
                        collect_result_type(&value.ty, &mut result_types)?;
                    }
                    IrInst::CellNew { init, .. } => {
                        collect_result_type(&init.ty, &mut result_types)?;
                    }
                    IrInst::CellStore { cell, value } => {
                        collect_result_type(&cell.ty, &mut result_types)?;
                        collect_result_type(&value.ty, &mut result_types)?;
                    }
                    IrInst::BeginTry { .. }
                    | IrInst::EndTry
                    | IrInst::BindError { .. }
                    | IrInst::DefineClosure { .. }
                    | IrInst::CellRelease(_)
                    | IrInst::Unsupported { .. } => {}
                }
            }
            match &block.terminator {
                IrTerminator::ResultBranch { result, .. }
                | IrTerminator::JumpErr { result, .. }
                | IrTerminator::PropagateErr(result)
                | IrTerminator::Return(Some(result)) => {
                    collect_result_type(&result.ty, &mut result_types)?;
                }
                IrTerminator::Branch { condition, .. } => {
                    collect_result_type(&condition.ty, &mut result_types)?;
                }
                IrTerminator::ForEach { iterable, .. } => {
                    collect_result_type(&iterable.ty, &mut result_types)?;
                }
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Return(None)
                | IrTerminator::Unreachable => {}
            }
        }
    }
    // Object programs always need Result<KuValue> for `ku_object_get_result`,
    // plus Result<int>/Result<str> for `ku_value_as_int`/`as_str`.
    if program_uses_object(program) {
        for forced in [
            IrType::Named("__ku_value".to_string()),
            IrType::Int,
            IrType::Str,
        ] {
            if !result_types.contains(&forced) {
                result_types.push(forced);
            }
        }
    }
    // The pg/redis runtimes emit their whole API unconditionally, so the Result types
    // their functions return must exist even if the program calls only some of them.
    let mut forced: Vec<IrType> = Vec::new();
    if program_uses_pg(program) {
        forced.push(IrType::Named("__ku_pg_conn".to_string()));
        forced.push(IrType::Named("__ku_pg_result".to_string()));
    }
    if program_uses_pg_pool(program) {
        forced.push(IrType::Named("__ku_pg_pool".to_string()));
    }
    if program_uses_redis(program) {
        forced.push(IrType::Named("__ku_redis_conn".to_string()));
        forced.push(IrType::Null);
        forced.push(IrType::Str);
        forced.push(IrType::Int);
    }
    if program_uses_mysql(program) {
        forced.push(IrType::Named("__ku_mysql_conn".to_string()));
        forced.push(IrType::Named("__ku_mysql_result".to_string()));
    }
    for t in forced {
        if !result_types.contains(&t) {
            result_types.push(t);
        }
    }
    // The HTTP runtime always references `KuResult_struct___ku_http_response`
    // (`ku_http_response_from_result` and the `returns_result` call paths), so
    // emit that Result even when no handler in the program returns a `Result`.
    if program_uses_http(program) {
        let inner = IrType::Named("__ku_http_response".to_string());
        if !result_types.contains(&inner) {
            result_types.push(inner);
        }
    }
    for inner in &result_types {
        let result_type = c_result_type(inner)?;
        let suffix = c_type_suffix(inner)?;
        let value_type = c_type(inner)?;
        out.push_str(&format!(
            "typedef struct {{ bool ok; {value_type} value; KuError error; }} {result_type};\n"
        ));
        out.push_str(&format!(
            "static {result_type} ku_result_move_{suffix}({result_type}* result) {{ {result_type} value = *result; *result = ({result_type}){{0}}; return value; }}\n"
        ));
        out.push_str(&format!(
            "static {value_type} ku_result_take_{suffix}({result_type}* result) {{ {value_type} value = result->value; result->value = {}; result->ok = false; return value; }}\n",
            c_zero_value(inner)?
        ));
        out.push_str(&format!(
            "static {result_type} ku_result_clone_{suffix}({result_type} result) {{ if (result.ok) result.value = {}; else result.error = ku_error_clone(result.error); return result; }}\n",
            c_clone_value(inner, "result.value")?
        ));
        out.push_str(&format!(
            "static void ku_result_drop_{suffix}({result_type}* result) {{ if (!result) return; if (result->ok) {{ {} }} else {{ ku_error_drop(&result->error); }} *result = ({result_type}){{0}}; }}\n",
            c_drop_value(inner, "result->value")?
        ));
    }
    if !result_types.is_empty() {
        out.push('\n');
    }
    Ok(())
}

/// Emit the tagged KuValue + open-addressing KuObject runtime, only when the
/// program actually uses dynamic objects. Depends on the KuString ABI already
/// emitted in the header.
fn emit_object_abi(out: &mut String, program: &IrProgram) -> KuResult<()> {
    if !program_uses_object(program) {
        return Ok(());
    }
    out.push_str(
        "typedef enum { KU_NULL=0, KU_INT, KU_BOOL, KU_STR, KU_OBJECT, KU_ARRAY, KU_FUNCTION } KuValueTag;\n\
         typedef struct KuValue KuValue;\n\
         typedef struct KuObject KuObject;\n\
         typedef struct KuValueArray KuValueArray;\n\
         struct KuValue { KuValueTag tag; union { int64_t i; bool b; KuString s; KuObject* o; KuValueArray* arr; struct { void* invoke; void* env; } fn; } as; };\n\
         typedef struct { KuString key; KuValue value; bool used; } KuEntry;\n\
         struct KuObject { size_t len; size_t cap; KuEntry* entries; };\n\
         struct KuValueArray { size_t len; size_t cap; KuValue* data; };\n\
         static KuValue ku_v_null(void) { KuValue v; v.tag=KU_NULL; v.as.i=0; return v; }\n\
         static KuValue ku_v_int(int64_t i) { KuValue v; v.tag=KU_INT; v.as.i=i; return v; }\n\
         static KuValue ku_v_bool(bool b) { KuValue v; v.tag=KU_BOOL; v.as.b=b; return v; }\n\
         static KuValue ku_v_str(KuString s) { KuValue v; v.tag=KU_STR; v.as.s=s; return v; }\n\
         static KuValue ku_v_object(KuObject* o) { KuValue v; v.tag=KU_OBJECT; v.as.o=o; return v; }\n\
         static KuValue ku_v_array(KuValueArray* a) { KuValue v; v.tag=KU_ARRAY; v.as.arr=a; return v; }\n\
         static KuValue ku_v_function(void* invoke, void* env) { KuValue v; v.tag=KU_FUNCTION; v.as.fn.invoke=invoke; v.as.fn.env=env; return v; }\n\
         static uint64_t ku_obj_hash(KuString k) { uint64_t h=1469598103934665603ULL; for (size_t i=0;i<k.len;i++) { h^=k.ptr[i]; h*=1099511628211ULL; } return h; }\n\
         static void ku_value_drop(KuValue* v);\n\
         static KuValue ku_value_clone(KuValue v);\n\
         static KuValueArray* ku_value_array_clone(KuValueArray* a);\n\
         static void ku_value_array_drop(KuValueArray* a);\n\
         static void ku_object_set(KuObject* o, KuString key, KuValue value);\n\
         static KuObject* ku_object_new(size_t cap) { KuObject* o=(KuObject*)malloc(sizeof(KuObject)); if (cap<8) cap=8; o->len=0; o->cap=cap; o->entries=(KuEntry*)calloc(cap,sizeof(KuEntry)); if (!o->entries) { fprintf(stderr, \"out of memory\\n\"); exit(1); } return o; }\n\
         static void ku_object_rehash(KuObject* o) { size_t oldcap=o->cap; KuEntry* old=o->entries; o->cap=oldcap*2; o->entries=(KuEntry*)calloc(o->cap,sizeof(KuEntry)); if (!o->entries) { fprintf(stderr, \"out of memory\\n\"); exit(1); } o->len=0; for (size_t i=0;i<oldcap;i++) if (old[i].used) ku_object_set(o, old[i].key, old[i].value); free(old); }\n\
         static void ku_object_set(KuObject* o, KuString key, KuValue value) { if ((o->len+1)*4 >= o->cap*3) ku_object_rehash(o); size_t mask=o->cap-1; size_t idx=(size_t)ku_obj_hash(key)&mask; while (o->entries[idx].used) { if (ku_string_equal(o->entries[idx].key, key)) { ku_string_drop(&key); ku_value_drop(&o->entries[idx].value); o->entries[idx].value=value; return; } idx=(idx+1)&mask; } o->entries[idx].key=key; o->entries[idx].value=value; o->entries[idx].used=true; o->len++; }\n\
         static KuValue* ku_object_get(KuObject* o, KuString key) { if (!o) return NULL; size_t mask=o->cap-1; size_t idx=(size_t)ku_obj_hash(key)&mask; while (o->entries[idx].used) { if (ku_string_equal(o->entries[idx].key, key)) return &o->entries[idx].value; idx=(idx+1)&mask; } return NULL; }\n\
         static KuObject* ku_object_clone(KuObject* o) { if (!o) return NULL; KuObject* n=ku_object_new(o->cap); for (size_t i=0;i<o->cap;i++) if (o->entries[i].used) ku_object_set(n, ku_string_clone(o->entries[i].key), ku_value_clone(o->entries[i].value)); return n; }\n\
         static void ku_object_drop(KuObject* o) { if (!o) return; for (size_t i=0;i<o->cap;i++) if (o->entries[i].used) { ku_string_drop(&o->entries[i].key); ku_value_drop(&o->entries[i].value); } free(o->entries); free(o); }\n\
         static KuObject* ku_object_move(KuObject** o) { KuObject* m=*o; *o=NULL; return m; }\n\
         static KuValue ku_value_clone(KuValue v) { switch (v.tag) { case KU_STR: return ku_v_str(ku_string_clone(v.as.s)); case KU_OBJECT: return ku_v_object(ku_object_clone(v.as.o)); case KU_ARRAY: return ku_v_array(ku_value_array_clone(v.as.arr)); case KU_FUNCTION: if (v.as.fn.env) ((KuEnvHeader*)v.as.fn.env)->retain(v.as.fn.env); return v; default: return v; } }\n\
         static void ku_value_drop(KuValue* v) { if (!v) return; switch (v->tag) { case KU_STR: ku_string_drop(&v->as.s); break; case KU_OBJECT: ku_object_drop(v->as.o); v->as.o=NULL; break; case KU_ARRAY: ku_value_array_drop(v->as.arr); v->as.arr=NULL; break; case KU_FUNCTION: if (v->as.fn.env) ((KuEnvHeader*)v->as.fn.env)->release(v->as.fn.env); v->as.fn.env=NULL; break; default: break; } v->tag=KU_NULL; v->as.i=0; }\n\
         static KuValue ku_value_move(KuValue* v) { KuValue m=*v; v->tag=KU_NULL; v->as.i=0; return m; }\n\
         static void ku_value_print(KuValue v) { switch (v.tag) { case KU_INT: printf(\"%lld\", (long long)v.as.i); break; case KU_BOOL: printf(\"%s\", v.as.b ? \"true\" : \"false\"); break; case KU_STR: printf(\"%.*s\", (int)v.as.s.len, (const char*)v.as.s.ptr); break; case KU_OBJECT: printf(\"[object]\"); break; case KU_ARRAY: printf(\"[array]\"); break; case KU_FUNCTION: printf(\"<function>\"); break; default: printf(\"null\"); break; } }\n\
         static KuValue ku_object_get_or(KuObject* o, KuString key, KuValue def) { KuValue* v = ku_object_get(o, key); if (v) { ku_value_drop(&def); return ku_value_clone(*v); } return def; }\n\
         static KuValueArray* ku_value_array_new(void) { KuValueArray* a=(KuValueArray*)malloc(sizeof(KuValueArray)); if (!a) { fprintf(stderr, \"out of memory\\n\"); exit(1); } a->len=0; a->cap=0; a->data=NULL; return a; }\n\
         static void ku_value_array_push(KuValueArray* a, KuValue v) { if (a->len+1 > a->cap) { size_t nc = a->cap ? a->cap*2 : 8; a->data=(KuValue*)realloc(a->data, nc*sizeof(KuValue)); if (!a->data) { fprintf(stderr, \"out of memory\\n\"); exit(1); } a->cap=nc; } a->data[a->len++]=v; }\n\
         static KuValueArray* ku_value_array_clone(KuValueArray* a) { if (!a) return NULL; KuValueArray* n=ku_value_array_new(); for (size_t i=0;i<a->len;i++) ku_value_array_push(n, ku_value_clone(a->data[i])); return n; }\n\
         static void ku_value_array_drop(KuValueArray* a) { if (!a) return; for (size_t i=0;i<a->len;i++) ku_value_drop(&a->data[i]); free(a->data); free(a); }\n\n"
    );
    Ok(())
}

/// Emit `ku_object_get_result` (strict `obj[key]` -> Result&lt;KuValue&gt;) after the
/// result ABI, since it depends on `KuResult_kuvalue`. Missing keys produce
/// `Err{domain:"object", code:"missing_key", message:"missing object key: <key>"}`.
fn emit_object_result_helpers(out: &mut String, program: &IrProgram) -> KuResult<()> {
    if !program_uses_object(program) {
        return Ok(());
    }
    out.push_str(
        "static KuResult_kuvalue ku_object_get_result(KuObject* o, KuString key) {\n\
         \x20 KuValue* found = ku_object_get(o, key);\n\
         \x20 if (found) return (KuResult_kuvalue){ true, ku_value_clone(*found), (KuError){0} };\n\
         \x20 KuString msg = ku_string_concat(ku_string_static((const uint8_t*)\"missing object key: \", 20), key);\n\
         \x20 return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(ku_string_static((const uint8_t*)\"object\", 6), ku_string_static((const uint8_t*)\"missing_key\", 11), msg) };\n\
         }\n\
         static KuResult_int ku_value_as_int(KuValue v) { if (v.tag == KU_INT) { int64_t i = v.as.i; ku_value_drop(&v); return (KuResult_int){ true, i, (KuError){0} }; } ku_value_drop(&v); return (KuResult_int){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"value\", 5), ku_string_static((const uint8_t*)\"type_mismatch\", 13), ku_string_static((const uint8_t*)\"expected int value\", 18)) }; }\n\
         static KuResult_str ku_value_as_str(KuValue v) { if (v.tag == KU_STR) { KuString s = v.as.s; v.tag=KU_NULL; v.as.i=0; return (KuResult_str){ true, s, (KuError){0} }; } ku_value_drop(&v); return (KuResult_str){ false, (KuString){0}, ku_error_make(ku_string_static((const uint8_t*)\"value\", 5), ku_string_static((const uint8_t*)\"type_mismatch\", 13), ku_string_static((const uint8_t*)\"expected str value\", 18)) }; }\n\
         typedef struct { char* data; size_t len; size_t cap; } KuStrBuf;\n\
         static void ku_strbuf_push(KuStrBuf* b, const char* s, size_t n) { if (b->len + n + 1 > b->cap) { size_t nc = b->cap ? b->cap : 64; while (nc < b->len + n + 1) nc *= 2; b->data = (char*)realloc(b->data, nc); if (!b->data) { fprintf(stderr, \"out of memory\\n\"); exit(1); } b->cap = nc; } if (n) memcpy(b->data + b->len, s, n); b->len += n; }\n\
         static void ku_strbuf_byte(KuStrBuf* b, int c) { char ch = (char)c; ku_strbuf_push(b, &ch, 1); }\n\
         static void ku_json_escape(KuStrBuf* b, KuString s) { ku_strbuf_byte(b, 34); for (size_t i = 0; i < s.len; i++) { unsigned c = s.ptr[i]; if (c == 34 || c == 92) { ku_strbuf_byte(b, 92); ku_strbuf_byte(b, (int)c); } else if (c == 10) { ku_strbuf_byte(b, 92); ku_strbuf_byte(b, 110); } else if (c == 9) { ku_strbuf_byte(b, 92); ku_strbuf_byte(b, 116); } else if (c == 13) { ku_strbuf_byte(b, 92); ku_strbuf_byte(b, 114); } else { ku_strbuf_byte(b, (int)c); } } ku_strbuf_byte(b, 34); }\n\
         static void ku_json_write(KuStrBuf* b, KuValue v) { char num[32]; switch (v.tag) { case KU_INT: { int n = snprintf(num, sizeof(num), \"%lld\", (long long)v.as.i); ku_strbuf_push(b, num, (size_t)n); break; } case KU_BOOL: ku_strbuf_push(b, v.as.b ? \"true\" : \"false\", v.as.b ? 4 : 5); break; case KU_STR: ku_json_escape(b, v.as.s); break; case KU_OBJECT: { ku_strbuf_byte(b, 123); KuObject* o = v.as.o; int first = 1; if (o) for (size_t i = 0; i < o->cap; i++) if (o->entries[i].used) { if (!first) ku_strbuf_byte(b, 44); first = 0; ku_json_escape(b, o->entries[i].key); ku_strbuf_byte(b, 58); ku_json_write(b, o->entries[i].value); } ku_strbuf_byte(b, 125); break; } case KU_ARRAY: { ku_strbuf_byte(b, 91); KuValueArray* a = v.as.arr; if (a) for (size_t i = 0; i < a->len; i++) { if (i) ku_strbuf_byte(b, 44); ku_json_write(b, a->data[i]); } ku_strbuf_byte(b, 93); break; } default: ku_strbuf_push(b, \"null\", 4); break; } }\n\
         static KuString ku_json_stringify(KuValue v) { KuStrBuf b = {0}; ku_json_write(&b, v); ku_value_drop(&v); if (!b.data) return ku_string_static((const uint8_t*)\"\", 0); return (KuString){ (uint8_t*)b.data, b.len, b.cap, KU_STRING_OWNED }; }\n\
         static KuResult_kuvalue ku_value_get_result(KuValue v, KuString key) { if (v.tag == KU_OBJECT) return ku_object_get_result(v.as.o, key); KuString msg = ku_string_concat(ku_string_static((const uint8_t*)\"missing object key: \", 20), key); return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(ku_string_static((const uint8_t*)\"object\", 6), ku_string_static((const uint8_t*)\"missing_key\", 11), msg) }; }\n\
         static KuResult_kuvalue ku_value_index_result(KuValue v, int64_t i) { if (v.tag == KU_ARRAY) { KuValueArray* a = v.as.arr; if (a && i >= 0 && (size_t)i < a->len) return (KuResult_kuvalue){ true, ku_value_clone(a->data[i]), (KuError){0} }; char num[32]; int n = snprintf(num, sizeof(num), \"%lld\", (long long)i); KuString msg = ku_string_concat(ku_string_static((const uint8_t*)\"array index out of bounds: \", 27), ku_string_static((const uint8_t*)num, (size_t)n)); return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(ku_string_static((const uint8_t*)\"array\", 5), ku_string_static((const uint8_t*)\"index_out_of_bounds\", 19), msg) }; } return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(ku_string_static((const uint8_t*)\"array\", 5), ku_string_static((const uint8_t*)\"not_an_array\", 12), ku_string_static((const uint8_t*)\"expected array value\", 20)) }; }\n\
         static void ku_json_skip_ws(const uint8_t** p, const uint8_t* end) { while (*p < end && (**p == 32 || **p == 9 || **p == 10 || **p == 13)) (*p)++; }\n\
         static KuValue ku_json_parse_value(const uint8_t** p, const uint8_t* end);\n\
         static KuString ku_json_parse_string(const uint8_t** p, const uint8_t* end) { if (*p < end && **p == 34) (*p)++; KuStrBuf b = {0}; while (*p < end && **p != 34) { if (**p == 92 && *p + 1 < end) { (*p)++; uint8_t c = **p; if (c == 110) ku_strbuf_byte(&b, 10); else if (c == 116) ku_strbuf_byte(&b, 9); else if (c == 114) ku_strbuf_byte(&b, 13); else ku_strbuf_byte(&b, (int)c); (*p)++; } else { ku_strbuf_byte(&b, (int)(**p)); (*p)++; } } if (*p < end) (*p)++; if (!b.data) return ku_string_static((const uint8_t*)\"\", 0); return (KuString){ (uint8_t*)b.data, b.len, b.cap, KU_STRING_OWNED }; }\n\
         static KuValue ku_json_parse_value(const uint8_t** p, const uint8_t* end) { ku_json_skip_ws(p, end); if (*p >= end) return ku_v_null(); uint8_t c = **p; if (c == 123) { (*p)++; KuObject* o = ku_object_new(0); ku_json_skip_ws(p, end); while (*p < end && **p != 125) { ku_json_skip_ws(p, end); KuString key = ku_json_parse_string(p, end); ku_json_skip_ws(p, end); if (*p < end && **p == 58) (*p)++; KuValue val = ku_json_parse_value(p, end); ku_object_set(o, key, val); ku_json_skip_ws(p, end); if (*p < end && **p == 44) (*p)++; ku_json_skip_ws(p, end); } if (*p < end) (*p)++; return ku_v_object(o); } if (c == 91) { (*p)++; KuValueArray* a = ku_value_array_new(); ku_json_skip_ws(p, end); if (*p < end && **p == 93) { (*p)++; return ku_v_array(a); } while (*p < end) { KuValue elem = ku_json_parse_value(p, end); ku_value_array_push(a, elem); ku_json_skip_ws(p, end); if (*p < end && **p == 44) { (*p)++; ku_json_skip_ws(p, end); continue; } if (*p < end && **p == 93) { (*p)++; break; } break; } return ku_v_array(a); } if (c == 34) return ku_v_str(ku_json_parse_string(p, end)); if (c == 116) { *p += (*p + 4 <= end) ? 4 : (end - *p); return ku_v_bool(true); } if (c == 102) { *p += (*p + 5 <= end) ? 5 : (end - *p); return ku_v_bool(false); } if (c == 110) { *p += (*p + 4 <= end) ? 4 : (end - *p); return ku_v_null(); } { const uint8_t* s = *p; while (*p < end && (**p == 45 || **p == 43 || **p == 46 || (**p >= 48 && **p <= 57) || **p == 101 || **p == 69)) (*p)++; char buf[32]; size_t n = (size_t)(*p - s); if (n >= sizeof(buf)) n = sizeof(buf) - 1; memcpy(buf, s, n); buf[n] = 0; return ku_v_int((int64_t)atoll(buf)); } }\n\
         static KuValue ku_json_parse(KuString text) { const uint8_t* p = text.ptr; const uint8_t* end = text.ptr + text.len; return ku_json_parse_value(&p, end); }\n\n"
    );
    Ok(())
}

/// Emit a dynamic object literal as statements building a KuObject*
/// (`ku_object_new` + one `ku_object_set` per field). Returns true when handled.
fn try_emit_object_construction(out: &mut String, target: &str, value: &IrExpr) -> KuResult<bool> {
    let IrExprKind::Call {
        kind: IrCallKind::Intrinsic(name),
        args,
        ..
    } = &value.kind
    else {
        return Ok(false);
    };
    if name != "__ku_object" {
        return Ok(false);
    }
    out.push_str(&format!("  {target} = ku_object_new(0);\n"));
    let mut i = 0;
    while i + 1 < args.len() {
        let key = &args[i];
        let field = &args[i + 1];
        out.push_str(&format!(
            "  ku_object_set({target}, {}, {});\n",
            c_expr(key)?,
            ku_value_wrap(&field.ty, &c_value_expr(field)?)?
        ));
        i += 2;
    }
    Ok(true)
}

/// Wrap a native value into a tagged KuValue by its IR type (object fields are
/// heterogeneous, so each is boxed into a KuValue).
fn ku_value_wrap(ty: &IrType, expr: &str) -> KuResult<String> {
    match ty {
        IrType::Str => Ok(format!("ku_v_str({expr})")),
        IrType::Int => Ok(format!("ku_v_int({expr})")),
        IrType::Bool => Ok(format!("ku_v_bool({expr})")),
        IrType::Null => Ok("ku_v_null()".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok(format!("ku_v_object({expr})")),
        // Stage 6e-4: a function value boxed into a dynamic object is a
        // KU_FUNCTION KuValue that owns the moved closure's env reference. The
        // per-signature wrapper evaluates `expr` once (it is usually a move).
        IrType::Closure { params, ret } => Ok(format!(
            "ku_v_closure_{}({expr})",
            closure_signature_suffix(params, ret)?
        )),
        _ => Err(unsupported(format!(
            "native dynamic object cannot hold a value of type {ty}"
        ))),
    }
}

/// Emit a `ku_v_closure_{suffix}` per closure signature that boxes a closure
/// struct into a KU_FUNCTION `KuValue` (single-evaluation of its argument). Only
/// emitted when the program uses dynamic objects, since it depends on `KuValue`.
fn emit_closure_value_wrappers(out: &mut String, program: &IrProgram) -> KuResult<()> {
    if !program_uses_object(program) {
        return Ok(());
    }
    let mut types = Vec::new();
    collect_closure_types_program(program, &mut types);
    let mut emitted = false;
    for ty in &types {
        let IrType::Closure { params, ret } = ty else {
            continue;
        };
        let suffix = closure_signature_suffix(params, ret)?;
        out.push_str(&format!(
            "static KuValue ku_v_closure_{suffix}(KuClosure_{suffix} c) {{ return ku_v_function((void*)c.invoke, c.env); }}\n"
        ));
        emitted = true;
    }
    if emitted {
        out.push('\n');
    }
    Ok(())
}

fn program_uses_object(program: &IrProgram) -> bool {
    // Stage 8b: every HTTP program carries `KuObject`-typed request fields
    // (`params`/`query`/`headers`), so the full object/KuValue runtime must be
    // emitted for the request struct to reference it, even when the program
    // never writes an object literal of its own.
    if program_uses_http(program) {
        return true;
    }
    program.functions.iter().any(|function| {
        ir_type_uses_object(&function.return_type)
            || function.params.iter().any(|p| ir_type_uses_object(&p.ty))
            || function
                .blocks
                .iter()
                .any(|block| block.instructions.iter().any(inst_uses_object))
    })
}

fn ir_type_uses_object(ty: &IrType) -> bool {
    match ty {
        IrType::Named(name) => name == "__ku_object",
        IrType::Array(inner) | IrType::Result(inner) => ir_type_uses_object(inner),
        _ => false,
    }
}

fn inst_uses_object(inst: &IrInst) -> bool {
    match inst {
        IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
            ir_type_uses_object(ty) || expr_uses_object(value)
        }
        IrInst::BindOk { ty, result, .. } => {
            ir_type_uses_object(ty) || expr_uses_object(result)
        }
        IrInst::Store { value, .. }
        | IrInst::Print(value)
        | IrInst::Expr(value)
        | IrInst::Fail(value)
        | IrInst::Panic(value) => expr_uses_object(value),
        _ => false,
    }
}

fn expr_uses_object(expr: &IrExpr) -> bool {
    if ir_type_uses_object(&expr.ty) {
        return true;
    }
    match &expr.kind {
        IrExprKind::Call { callee, args, kind } => {
            matches!(kind, IrCallKind::Intrinsic(name)
                if name == "__ku_object" || name == "json.stringify" || name == "json.parse")
                || expr_uses_object(callee)
                || args.iter().any(expr_uses_object)
        }
        IrExprKind::Binary { left, right, .. } => {
            expr_uses_object(left) || expr_uses_object(right)
        }
        IrExprKind::Unary { expr, .. } => expr_uses_object(expr),
        IrExprKind::Index { target, index } => {
            expr_uses_object(target) || expr_uses_object(index)
        }
        IrExprKind::Field { target, .. } => expr_uses_object(target),
        IrExprKind::Array(values) => values.iter().any(expr_uses_object),
        IrExprKind::StructLiteral { fields, .. } => {
            fields.iter().any(|(_, v)| expr_uses_object(v))
        }
        IrExprKind::TryUnwrap(inner) => expr_uses_object(inner),
        _ => false,
    }
}

/// Stage 8a: true when the program uses the native HTTP server (so the winsock
/// runtime, response/request structs, and the `ws2_32` link are emitted). The
/// synthetic HTTP types only arise from the HTTP intrinsics, so a type scan over
/// every temp/param/return type catches every such program.
fn program_uses_http(program: &IrProgram) -> bool {
    program.functions.iter().any(|function| {
        ir_type_uses_http(&function.return_type)
            || function.params.iter().any(|p| ir_type_uses_http(&p.ty))
            || function.blocks.iter().any(|block| {
                block.instructions.iter().any(inst_uses_http)
            })
    })
}

/// True when any type in the program is a `pg` handle — the only way those arise is a
/// `pg` intrinsic, so a type scan detects every program that needs the libpq binding.
fn program_uses_pg(program: &IrProgram) -> bool {
    fn ty_uses_pg(ty: &IrType) -> bool {
        match ty {
            IrType::Named(name) => name.starts_with("__ku_pg_"),
            IrType::Array(inner) | IrType::Result(inner) | IrType::Cell(inner) => ty_uses_pg(inner),
            IrType::Closure { params, ret } => {
                params.iter().any(ty_uses_pg) || ty_uses_pg(ret)
            }
            _ => false,
        }
    }
    fn inst_uses_pg(inst: &IrInst) -> bool {
        match inst {
            IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
                ty_uses_pg(ty) || ty_uses_pg(&value.ty)
            }
            IrInst::BindOk { ty, result, .. } => ty_uses_pg(ty) || ty_uses_pg(&result.ty),
            IrInst::Store { value, .. }
            | IrInst::Print(value)
            | IrInst::Expr(value)
            | IrInst::Fail(value)
            | IrInst::Panic(value) => ty_uses_pg(&value.ty),
            _ => false,
        }
    }
    program.functions.iter().any(|function| {
        ty_uses_pg(&function.return_type)
            || function.params.iter().any(|p| ty_uses_pg(&p.ty))
            || function.blocks.iter().any(|block| block.instructions.iter().any(inst_uses_pg))
    })
}

/// True when the program uses a `mysql` handle (needs the libmysqlclient binding).
fn program_uses_mysql(program: &IrProgram) -> bool {
    fn ty(t: &IrType) -> bool {
        match t {
            IrType::Named(name) => name.starts_with("__ku_mysql_"),
            IrType::Array(i) | IrType::Result(i) | IrType::Cell(i) => ty(i),
            IrType::Closure { params, ret } => params.iter().any(ty) || ty(ret),
            _ => false,
        }
    }
    fn inst(i: &IrInst) -> bool {
        match i {
            IrInst::Temp { ty: t, value, .. } | IrInst::Let { ty: t, value, .. } => {
                ty(t) || ty(&value.ty)
            }
            IrInst::BindOk { ty: t, result, .. } => ty(t) || ty(&result.ty),
            IrInst::Store { value, .. }
            | IrInst::Print(value)
            | IrInst::Expr(value)
            | IrInst::Fail(value)
            | IrInst::Panic(value) => ty(&value.ty),
            _ => false,
        }
    }
    program.functions.iter().any(|f| {
        ty(&f.return_type)
            || f.params.iter().any(|p| ty(&p.ty))
            || f.blocks.iter().any(|b| b.instructions.iter().any(inst))
    })
}

/// Forward-declare the opaque libmysqlclient types before the Result ABI (which
/// embeds `MYSQL*`/`MYSQL_RES*` in `KuResult_mysql_conn`/`KuResult_mysql_result`).
fn emit_mysql_types(out: &mut String, program: &IrProgram) {
    if !program_uses_mysql(program) {
        return;
    }
    out.push_str(concat!(
        "typedef struct MYSQL MYSQL;\ntypedef struct MYSQL_RES MYSQL_RES;\n",
        "static MYSQL* ku_move_mysql_conn(MYSQL** p);\n",
        "static void ku_drop_mysql_conn(MYSQL** p);\n",
        "static MYSQL* ku_clone_mysql_conn(MYSQL* c);\n",
        "static MYSQL_RES* ku_move_mysql_result(MYSQL_RES** p);\n",
        "static void ku_drop_mysql_result(MYSQL_RES** p);\n",
        "static MYSQL_RES* ku_clone_mysql_result(MYSQL_RES* r);\n\n",
    ));
}

/// Emit the `mysql` runtime: a thin libmysqlclient binding. Values come back as text.
/// `query_params` builds the SQL by escaping each `?` param with
/// `mysql_real_escape_string` (injection-safe). The handle owns the connection/result;
/// drop closes/frees it.
fn emit_mysql_runtime(out: &mut String, program: &IrProgram) {
    if !program_uses_mysql(program) {
        return;
    }
    out.push_str(concat!(
        "#if defined(_MSC_VER)\n#pragma comment(lib, \"libmysql.lib\")\n#endif\n",
        "typedef char** MYSQL_ROW;\n",
        "extern MYSQL* mysql_init(MYSQL*);\n",
        "extern MYSQL* mysql_real_connect(MYSQL*, const char*, const char*, const char*, const char*, unsigned int, const char*, unsigned long);\n",
        "extern int mysql_query(MYSQL*, const char*);\n",
        "extern MYSQL_RES* mysql_store_result(MYSQL*);\n",
        "extern unsigned long long mysql_num_rows(MYSQL_RES*);\n",
        "extern unsigned int mysql_num_fields(MYSQL_RES*);\n",
        "extern void mysql_data_seek(MYSQL_RES*, unsigned long long);\n",
        "extern MYSQL_ROW mysql_fetch_row(MYSQL_RES*);\n",
        "extern void mysql_free_result(MYSQL_RES*);\n",
        "extern void mysql_close(MYSQL*);\n",
        "extern const char* mysql_error(MYSQL*);\n",
        "extern unsigned long mysql_real_escape_string(MYSQL*, char*, const char*, unsigned long);\n",
        "static KuString ku_mysql_copy(const char* s) {\n",
        "  if (!s) return (KuString){0}; size_t n = strlen(s); if (n == 0) return (KuString){0};\n",
        "  uint8_t* d = (uint8_t*)malloc(n); if (!d) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
        "  memcpy(d, s, n); return (KuString){ d, n, n, KU_STRING_OWNED };\n",
        "}\n",
        "static KuError ku_mysql_err(const char* m) { return ku_error_make(ku_string_static((const uint8_t*)\"mysql\", 5), ku_string_static((const uint8_t*)\"mysql_error\", 11), ku_mysql_copy(m)); }\n",
        "static MYSQL* ku_move_mysql_conn(MYSQL** p) { MYSQL* m = *p; *p = 0; return m; }\n",
        "static void ku_drop_mysql_conn(MYSQL** p) { if (p && *p) { mysql_close(*p); *p = 0; } }\n",
        "static MYSQL* ku_clone_mysql_conn(MYSQL* c) { (void)c; fprintf(stderr, \"cannot clone a mysql connection\\n\"); exit(1); }\n",
        "static MYSQL_RES* ku_move_mysql_result(MYSQL_RES** p) { MYSQL_RES* m = *p; *p = 0; return m; }\n",
        "static void ku_drop_mysql_result(MYSQL_RES** p) { if (p && *p) { mysql_free_result(*p); *p = 0; } }\n",
        "static MYSQL_RES* ku_clone_mysql_result(MYSQL_RES* r) { (void)r; fprintf(stderr, \"cannot clone a mysql result\\n\"); exit(1); }\n",
        "static KuResult_mysql_conn ku_mysql_connect(KuString host, int64_t port, KuString user, KuString password, KuString db) {\n",
        // NOTE: mysql_init lazily allocates a fixed one-time client-library global
        // state (not per-connection). MySQL 8.0's libmysql does not export
        // mysql_library_end, so that constant allocation is reclaimed by the OS at
        // process exit — it does not grow and is not a per-connection/query leak.
        "  MYSQL* c = mysql_init(0);\n",
        "  if (!c) return (KuResult_mysql_conn){ false, 0, ku_mysql_err(\"mysql_init failed\") };\n",
        "  char* h = ku_string_to_cstr(host); char* u = ku_string_to_cstr(user); char* p = ku_string_to_cstr(password); char* d = ku_string_to_cstr(db);\n",
        "  MYSQL* r = mysql_real_connect(c, h, u, p, d, (unsigned int)port, 0, 0);\n",
        "  free(h); free(u); free(p); free(d);\n",
        "  if (!r) { KuString e = ku_mysql_copy(mysql_error(c)); mysql_close(c); return (KuResult_mysql_conn){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"mysql\", 5), ku_string_static((const uint8_t*)\"connect_error\", 13), e) }; }\n",
        "  return (KuResult_mysql_conn){ true, c, (KuError){0} };\n",
        "}\n",
        "static KuResult_mysql_result ku_mysql_run(MYSQL* c, char* q) {\n",
        "  if (mysql_query(c, q) != 0) return (KuResult_mysql_result){ false, 0, ku_mysql_err(mysql_error(c)) };\n",
        "  MYSQL_RES* res = mysql_store_result(c);\n",
        "  if (!res && mysql_error(c)[0] != 0) return (KuResult_mysql_result){ false, 0, ku_mysql_err(mysql_error(c)) };\n",
        "  return (KuResult_mysql_result){ true, res, (KuError){0} };\n",  // res may be NULL for non-SELECT (0 rows)
        "}\n",
        "static KuResult_mysql_result ku_mysql_query(MYSQL* c, KuString sql) {\n",
        "  if (!c) return (KuResult_mysql_result){ false, 0, ku_mysql_err(\"connection is closed\") };\n",
        "  char* q = ku_string_to_cstr(sql); KuResult_mysql_result r = ku_mysql_run(c, q); free(q); return r;\n",
        "}\n",
        // Injection-safe: replace each `?` with the next param, escaped and single-quoted.
        "static KuResult_mysql_result ku_mysql_query_params(MYSQL* c, KuString sql, KuArray_str params) {\n",
        "  if (!c) return (KuResult_mysql_result){ false, 0, ku_mysql_err(\"connection is closed\") };\n",
        "  size_t cap = sql.len + 16, len = 0; char* out = (char*)malloc(cap); if (!out) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
        "  size_t pi = 0;\n",
        "  for (size_t i = 0; i < sql.len; i++) {\n",
        "    char ch = (char)sql.ptr[i];\n",
        "    if (ch == '?' && pi < params.len) {\n",
        "      KuString a = params.data[pi++];\n",
        "      size_t need = len + a.len * 2 + 3;\n",
        "      if (need > cap) { while (cap < need) cap *= 2; out = (char*)realloc(out, cap); if (!out) { fprintf(stderr, \"out of memory\\n\"); exit(1); } }\n",
        "      out[len++] = '\\'';\n",
        "      char* from = ku_string_to_cstr(a);\n",
        "      unsigned long el = mysql_real_escape_string(c, out + len, from, (unsigned long)a.len);\n",
        "      free(from); len += el; out[len++] = '\\'';\n",
        "    } else {\n",
        "      if (len + 1 > cap) { cap *= 2; out = (char*)realloc(out, cap); if (!out) { fprintf(stderr, \"out of memory\\n\"); exit(1); } }\n",
        "      out[len++] = ch;\n",
        "    }\n",
        "  }\n",
        "  if (len + 1 > cap) { char* nb = (char*)realloc(out, len + 1); if (!nb) { free(out); fprintf(stderr, \"out of memory\\n\"); exit(1); } out = nb; }\n",
        "  out[len] = 0;\n",
        "  KuResult_mysql_result r = ku_mysql_run(c, out); free(out); return r;\n",
        "}\n",
        "static int64_t ku_mysql_rows(MYSQL_RES* r) { return r ? (int64_t)mysql_num_rows(r) : 0; }\n",
        "static int64_t ku_mysql_cols(MYSQL_RES* r) { return r ? (int64_t)mysql_num_fields(r) : 0; }\n",
        "static KuString ku_mysql_value(MYSQL_RES* r, int64_t row, int64_t col) {\n",
        "  if (!r) return (KuString){0};\n",
        "  mysql_data_seek(r, (unsigned long long)row);\n",
        "  MYSQL_ROW rr = mysql_fetch_row(r);\n",
        "  if (!rr || col < 0 || col >= (int64_t)mysql_num_fields(r)) return (KuString){0};\n",
        "  return ku_mysql_copy(rr[col]);\n",
        "}\n",
        "static uint8_t ku_mysql_close(MYSQL* c) { if (c) mysql_close(c); return 0; }\n\n",
    ));
}

/// True when the program uses a `redis` connection handle (needs the RESP runtime).
fn program_uses_redis(program: &IrProgram) -> bool {
    fn ty(t: &IrType) -> bool {
        match t {
            IrType::Named(name) => name == "__ku_redis_conn",
            IrType::Array(i) | IrType::Result(i) | IrType::Cell(i) => ty(i),
            IrType::Closure { params, ret } => params.iter().any(ty) || ty(ret),
            _ => false,
        }
    }
    fn inst(i: &IrInst) -> bool {
        match i {
            IrInst::Temp { ty: t, value, .. } | IrInst::Let { ty: t, value, .. } => {
                ty(t) || ty(&value.ty)
            }
            IrInst::BindOk { ty: t, result, .. } => ty(t) || ty(&result.ty),
            IrInst::Store { value, .. }
            | IrInst::Print(value)
            | IrInst::Expr(value)
            | IrInst::Fail(value)
            | IrInst::Panic(value) => ty(&value.ty),
            _ => false,
        }
    }
    program.functions.iter().any(|f| {
        ty(&f.return_type)
            || f.params.iter().any(|p| ty(&p.ty))
            || f.blocks.iter().any(|b| b.instructions.iter().any(inst))
    })
}

/// Forward-declare the opaque `KuRedis` handle (a socket wrapper) before the Result
/// ABI, since `KuResult_redis_conn` embeds a `KuRedis*`.
fn emit_redis_types(out: &mut String, program: &IrProgram) {
    if !program_uses_redis(program) {
        return;
    }
    out.push_str(concat!(
        "typedef struct KuRedis KuRedis;\n",
        "static KuRedis* ku_move_redis_conn(KuRedis** p);\n",
        "static void ku_drop_redis_conn(KuRedis** p);\n",
        "static KuRedis* ku_clone_redis_conn(KuRedis* c);\n\n",
    ));
}

/// Emit the `redis` runtime: a minimal RESP client over Winsock (no external lib).
/// Supports connect/auth/get/set/del/close. Replies are parsed for the four RESP
/// types the commands use (+simple, -error, :integer, $bulk). The handle owns a
/// socket; drop closes it.
fn emit_redis_runtime(out: &mut String, program: &IrProgram) {
    if !program_uses_redis(program) {
        return;
    }
    out.push_str(concat!(
        "struct KuRedis { SOCKET sock; };\n",
        "static int ku_redis_wsa_started = 0;\n",
        "static KuString ku_redis_copy(const char* s, size_t n) {\n",
        "  if (!s || n == 0) return (KuString){0};\n",
        "  uint8_t* d = (uint8_t*)malloc(n);\n",
        "  if (!d) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
        "  memcpy(d, s, n);\n",
        "  return (KuString){ d, n, n, KU_STRING_OWNED };\n",
        "}\n",
        "static KuError ku_redis_err(const char* m) { return ku_error_make(ku_string_static((const uint8_t*)\"redis\", 5), ku_string_static((const uint8_t*)\"redis_error\", 11), ku_redis_copy(m, strlen(m))); }\n",
        "static KuRedis* ku_move_redis_conn(KuRedis** p) { KuRedis* m = *p; *p = 0; return m; }\n",
        "static void ku_drop_redis_conn(KuRedis** p) { if (p && *p) { closesocket((*p)->sock); free(*p); *p = 0; } }\n",
        "static KuRedis* ku_clone_redis_conn(KuRedis* c) { (void)c; fprintf(stderr, \"cannot clone a redis connection\\n\"); exit(1); }\n",
        "static int ku_redis_send_all(SOCKET s, const char* d, size_t len) {\n",
        "  size_t sent = 0;\n",
        "  while (sent < len) { int n = send(s, d + sent, (int)(len - sent), 0); if (n <= 0) return -1; sent += (size_t)n; }\n",
        "  return 0;\n",
        "}\n",
        "static int ku_redis_send_cmd(SOCKET s, int argc, const KuString* args) {\n",
        "  char hdr[32]; int n = snprintf(hdr, sizeof(hdr), \"*%d\\r\\n\", argc);\n",
        "  if (ku_redis_send_all(s, hdr, (size_t)n) != 0) return -1;\n",
        "  for (int i = 0; i < argc; i++) {\n",
        "    n = snprintf(hdr, sizeof(hdr), \"$%zu\\r\\n\", args[i].len);\n",
        "    if (ku_redis_send_all(s, hdr, (size_t)n) != 0) return -1;\n",
        "    if (args[i].len && ku_redis_send_all(s, (const char*)args[i].ptr, args[i].len) != 0) return -1;\n",
        "    if (ku_redis_send_all(s, \"\\r\\n\", 2) != 0) return -1;\n",
        "  }\n",
        "  return 0;\n",
        "}\n",
        "static int ku_redis_read_line(SOCKET s, char* buf, int cap) {\n",
        "  int n = 0;\n",
        "  for (;;) { char c; int got = recv(s, &c, 1, 0); if (got <= 0) return -1; if (c == '\\r') { char lf; recv(s, &lf, 1, 0); break; } if (n < cap - 1) buf[n++] = c; }\n",
        "  buf[n] = 0; return n;\n",
        "}\n",
        "static int ku_redis_read_bytes(SOCKET s, char* buf, int len) {\n",
        "  int got = 0;\n",
        "  while (got < len) { int n = recv(s, buf + got, len - got, 0); if (n <= 0) return -1; got += n; }\n",
        "  return 0;\n",
        "}\n",
        "static KuResult_redis_conn ku_redis_connect(KuString host, int64_t port) {\n",
        "#if defined(_WIN32)\n",
        "  if (!ku_redis_wsa_started) { WSADATA w; if (WSAStartup(MAKEWORD(2,2), &w) != 0) return (KuResult_redis_conn){ false, 0, ku_redis_err(\"WSAStartup failed\") }; ku_redis_wsa_started = 1; }\n",
        "#endif\n",
        "  char* h = ku_string_to_cstr(host);\n",
        "  char ps[16]; snprintf(ps, sizeof(ps), \"%lld\", (long long)port);\n",
        "  struct addrinfo hints; memset(&hints, 0, sizeof(hints)); hints.ai_family = AF_INET; hints.ai_socktype = SOCK_STREAM;\n",
        "  struct addrinfo* res = 0; int rc = getaddrinfo(h, ps, &hints, &res); free(h);\n",
        "  if (rc != 0 || !res) return (KuResult_redis_conn){ false, 0, ku_redis_err(\"host resolution failed\") };\n",
        "  SOCKET sock = socket(res->ai_family, res->ai_socktype, res->ai_protocol);\n",
        "  if (sock == INVALID_SOCKET) { freeaddrinfo(res); return (KuResult_redis_conn){ false, 0, ku_redis_err(\"socket failed\") }; }\n",
        "  if (connect(sock, res->ai_addr, (int)res->ai_addrlen) != 0) { freeaddrinfo(res); closesocket(sock); return (KuResult_redis_conn){ false, 0, ku_redis_err(\"connect failed\") }; }\n",
        "  freeaddrinfo(res);\n",
        "  KuRedis* r = (KuRedis*)malloc(sizeof(KuRedis)); if (!r) { closesocket(sock); fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
        "  r->sock = sock; return (KuResult_redis_conn){ true, r, (KuError){0} };\n",
        "}\n",
        "static KuResult_null ku_redis_simple(KuRedis* r, int argc, const KuString* args) {\n",
        "  if (!r) return (KuResult_null){ false, 0, ku_redis_err(\"connection is closed\") };\n",
        "  if (ku_redis_send_cmd(r->sock, argc, args) != 0) return (KuResult_null){ false, 0, ku_redis_err(\"send failed\") };\n",
        "  char line[1024]; int n = ku_redis_read_line(r->sock, line, sizeof(line));\n",
        "  if (n < 0) return (KuResult_null){ false, 0, ku_redis_err(\"read failed\") };\n",
        "  if (line[0] == '+') return (KuResult_null){ true, 0, (KuError){0} };\n",
        "  return (KuResult_null){ false, 0, ku_redis_err(line[0] == '-' ? line + 1 : line) };\n",
        "}\n",
        "static KuResult_null ku_redis_auth(KuRedis* r, KuString password) {\n",
        "  KuString a[2] = { ku_string_static((const uint8_t*)\"AUTH\", 4), password };\n",
        "  return ku_redis_simple(r, 2, a);\n",
        "}\n",
        "static KuResult_null ku_redis_set(KuRedis* r, KuString key, KuString val) {\n",
        "  KuString a[3] = { ku_string_static((const uint8_t*)\"SET\", 3), key, val };\n",
        "  return ku_redis_simple(r, 3, a);\n",
        "}\n",
        "static KuResult_str ku_redis_get(KuRedis* r, KuString key) {\n",
        "  if (!r) return (KuResult_str){ false, (KuString){0}, ku_redis_err(\"connection is closed\") };\n",
        "  KuString a[2] = { ku_string_static((const uint8_t*)\"GET\", 3), key };\n",
        "  if (ku_redis_send_cmd(r->sock, 2, a) != 0) return (KuResult_str){ false, (KuString){0}, ku_redis_err(\"send failed\") };\n",
        "  char line[1024]; int n = ku_redis_read_line(r->sock, line, sizeof(line));\n",
        "  if (n < 0) return (KuResult_str){ false, (KuString){0}, ku_redis_err(\"read failed\") };\n",
        "  if (line[0] == '-') return (KuResult_str){ false, (KuString){0}, ku_redis_err(line + 1) };\n",
        "  if (line[0] != '$') return (KuResult_str){ false, (KuString){0}, ku_redis_err(\"unexpected reply\") };\n",
        "  long len = atol(line + 1);\n",
        "  if (len < 0) return (KuResult_str){ true, (KuString){0}, (KuError){0} };\n",
        "  char* buf = (char*)malloc((size_t)(len ? len : 1));\n",
        "  if (!buf) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
        "  if (ku_redis_read_bytes(r->sock, buf, (int)len) != 0) { free(buf); return (KuResult_str){ false, (KuString){0}, ku_redis_err(\"read failed\") }; }\n",
        "  char crlf[2]; ku_redis_read_bytes(r->sock, crlf, 2);\n",
        "  KuString v = ku_redis_copy(buf, (size_t)len); free(buf);\n",
        "  return (KuResult_str){ true, v, (KuError){0} };\n",
        "}\n",
        "static KuResult_int ku_redis_del(KuRedis* r, KuString key) {\n",
        "  if (!r) return (KuResult_int){ false, 0, ku_redis_err(\"connection is closed\") };\n",
        "  KuString a[2] = { ku_string_static((const uint8_t*)\"DEL\", 3), key };\n",
        "  if (ku_redis_send_cmd(r->sock, 2, a) != 0) return (KuResult_int){ false, 0, ku_redis_err(\"send failed\") };\n",
        "  char line[1024]; int n = ku_redis_read_line(r->sock, line, sizeof(line));\n",
        "  if (n < 0) return (KuResult_int){ false, 0, ku_redis_err(\"read failed\") };\n",
        "  if (line[0] == ':') return (KuResult_int){ true, (int64_t)atoll(line + 1), (KuError){0} };\n",
        "  return (KuResult_int){ false, 0, ku_redis_err(line[0] == '-' ? line + 1 : line) };\n",
        "}\n",
        "static uint8_t ku_redis_close(KuRedis* r) { if (r) { closesocket(r->sock); free(r); } return 0; }\n\n",
    ));
}

/// True when the program uses a `pg` connection pool (needs CRITICAL_SECTION etc.).
fn program_uses_pg_pool(program: &IrProgram) -> bool {
    fn ty(t: &IrType) -> bool {
        match t {
            IrType::Named(name) => name == "__ku_pg_pool",
            IrType::Array(i) | IrType::Result(i) | IrType::Cell(i) => ty(i),
            IrType::Closure { params, ret } => params.iter().any(ty) || ty(ret),
            _ => false,
        }
    }
    fn inst(i: &IrInst) -> bool {
        match i {
            IrInst::Temp { ty: t, value, .. } | IrInst::Let { ty: t, value, .. } => {
                ty(t) || ty(&value.ty)
            }
            IrInst::BindOk { ty: t, result, .. } => ty(t) || ty(&result.ty),
            IrInst::Store { value, .. }
            | IrInst::Print(value)
            | IrInst::Expr(value)
            | IrInst::Fail(value)
            | IrInst::Panic(value) => ty(&value.ty),
            _ => false,
        }
    }
    program.functions.iter().any(|f| {
        ty(&f.return_type)
            || f.params.iter().any(|p| ty(&p.ty))
            || f.blocks.iter().any(|b| b.instructions.iter().any(inst))
    })
}

/// Forward-declare the opaque libpq types. Emitted before the Result ABI, since
/// `KuResult_pg_conn` embeds a `PGconn*` field (a pointer to an incomplete type is
/// fine, but the tag must be declared).
fn emit_pg_types(out: &mut String, program: &IrProgram) {
    if !program_uses_pg(program) {
        return;
    }
    out.push_str(concat!(
        "typedef struct pg_conn PGconn;\ntypedef struct pg_result PGresult;\n",
        "typedef struct KuPgPool KuPgPool;\n",
        // The Result ABI (emitted next) calls these clone/drop helpers, which are
        // defined later in `emit_pg_runtime`; forward-declare them here.
        "static PGconn* ku_move_pg_conn(PGconn** p);\n",
        "static void ku_drop_pg_conn(PGconn** p);\n",
        "static PGconn* ku_clone_pg_conn(PGconn* c);\n",
        "static PGresult* ku_move_pg_result(PGresult** p);\n",
        "static void ku_drop_pg_result(PGresult** p);\n",
        "static PGresult* ku_clone_pg_result(PGresult* r);\n",
        "static KuPgPool* ku_move_pg_pool(KuPgPool** p);\n",
        "static void ku_drop_pg_pool(KuPgPool** p);\n",
        "static KuPgPool* ku_clone_pg_pool(KuPgPool* c);\n\n",
    ));
}

/// Emit the `pg` (thin libpq binding) runtime: link pragma, `PQ*` prototypes, the
/// opaque-handle move/clone/drop helpers (drop closes/frees the C resource), and the
/// `ku_pg_*` API. Emitted after the Result ABI so `KuResult_pg_conn/pg_result` exist.
/// Values come back as text (libpq text mode); each `pg.value` returns a fresh owned
/// copy so the result set can be freed independently.
fn emit_pg_runtime(out: &mut String, program: &IrProgram) {
    if !program_uses_pg(program) {
        return;
    }
    out.push_str(concat!(
        "#if defined(_MSC_VER)\n",
        // The library NAME is fixed; the CLI supplies the search path (/LIBPATH) from
        // KU_PG_LIB / `pg_config --libdir` / a default, so no absolute path is baked in.
        "#pragma comment(lib, \"libpq.lib\")\n",
        "#endif\n",
        "extern PGconn* PQconnectdb(const char*);\n",
        "extern int PQstatus(const PGconn*);\n",
        "extern char* PQerrorMessage(const PGconn*);\n",
        "extern void PQfinish(PGconn*);\n",
        "extern PGresult* PQexec(PGconn*, const char*);\n",
        "extern PGresult* PQexecParams(PGconn*, const char*, int, const void*, const char* const*, const int*, const int*, int);\n",
        "extern int PQresultStatus(const PGresult*);\n",
        "extern char* PQresultErrorMessage(const PGresult*);\n",
        "extern int PQntuples(const PGresult*);\n",
        "extern int PQnfields(const PGresult*);\n",
        "extern char* PQgetvalue(const PGresult*, int, int);\n",
        "extern void PQclear(PGresult*);\n",
        "static KuString ku_pg_copy_cstr(const char* s) {\n",
        "  if (!s) return (KuString){0};\n",
        "  size_t n = strlen(s);\n",
        "  if (n == 0) return (KuString){0};\n",
        "  uint8_t* d = (uint8_t*)malloc(n);\n",
        "  if (!d) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
        "  memcpy(d, s, n);\n",
        "  return (KuString){ d, n, n, KU_STRING_OWNED };\n",
        "}\n",
        "static PGconn* ku_move_pg_conn(PGconn** p) { PGconn* m = *p; *p = 0; return m; }\n",
        "static void ku_drop_pg_conn(PGconn** p) { if (p && *p) { PQfinish(*p); *p = 0; } }\n",
        "static PGconn* ku_clone_pg_conn(PGconn* c) { (void)c; fprintf(stderr, \"cannot clone a pg connection\\n\"); exit(1); }\n",
        "static PGresult* ku_move_pg_result(PGresult** p) { PGresult* m = *p; *p = 0; return m; }\n",
        "static void ku_drop_pg_result(PGresult** p) { if (p && *p) { PQclear(*p); *p = 0; } }\n",
        "static PGresult* ku_clone_pg_result(PGresult* r) { (void)r; fprintf(stderr, \"cannot clone a pg result\\n\"); exit(1); }\n",
        "static KuResult_pg_conn ku_pg_connect(KuString conninfo) {\n",
        "  char* ci = ku_string_to_cstr(conninfo);\n",
        "  PGconn* h = PQconnectdb(ci);\n",
        "  free(ci);\n",
        "  if (!h || PQstatus(h) != 0) {\n",
        "    KuString msg = ku_pg_copy_cstr(h ? PQerrorMessage(h) : \"connection allocation failed\");\n",
        "    if (h) PQfinish(h);\n",
        "    return (KuResult_pg_conn){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"connect_error\", 13), msg) };\n",
        "  }\n",
        "  return (KuResult_pg_conn){ true, h, (KuError){0} };\n",
        "}\n",
        "static KuResult_pg_result ku_pg_query(PGconn* conn, KuString sql) {\n",
        "  if (!conn) return (KuResult_pg_result){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"query_error\", 11), ku_string_static((const uint8_t*)\"connection is closed\", 20)) };\n",
        "  char* q = ku_string_to_cstr(sql);\n",
        "  PGresult* r = PQexec(conn, q);\n",
        "  free(q);\n",
        "  int st = r ? PQresultStatus(r) : 0;\n",
        "  if (!r || (st != 1 && st != 2)) {\n",
        "    KuString msg = ku_pg_copy_cstr(r ? PQresultErrorMessage(r) : \"query failed\");\n",
        "    if (r) PQclear(r);\n",
        "    return (KuResult_pg_result){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"query_error\", 11), msg) };\n",
        "  }\n",
        "  return (KuResult_pg_result){ true, r, (KuError){0} };\n",
        "}\n",
        // Parameterized query — the ONLY injection-safe path. `$1`..`$N` placeholders
        // bind to the params array (all text; libpq escapes them server-side).
        "static KuResult_pg_result ku_pg_query_params(PGconn* conn, KuString sql, KuArray_str params) {\n",
        "  if (!conn) return (KuResult_pg_result){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"query_error\", 11), ku_string_static((const uint8_t*)\"connection is closed\", 20)) };\n",
        "  char* q = ku_string_to_cstr(sql);\n",
        "  size_t n = params.len;\n",
        "  const char** values = 0;\n",
        "  if (n > 0) {\n",
        "    values = (const char**)malloc(n * sizeof(char*));\n",
        "    if (!values) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
        "    for (size_t i = 0; i < n; i++) values[i] = ku_string_to_cstr(params.data[i]);\n",
        "  }\n",
        "  PGresult* r = PQexecParams(conn, q, (int)n, 0, values, 0, 0, 0);\n",
        "  free(q);\n",
        "  if (values) { for (size_t i = 0; i < n; i++) free((void*)values[i]); free(values); }\n",
        "  int st = r ? PQresultStatus(r) : 0;\n",
        "  if (!r || (st != 1 && st != 2)) {\n",
        "    KuString msg = ku_pg_copy_cstr(r ? PQresultErrorMessage(r) : \"query failed\");\n",
        "    if (r) PQclear(r);\n",
        "    return (KuResult_pg_result){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"query_error\", 11), msg) };\n",
        "  }\n",
        "  return (KuResult_pg_result){ true, r, (KuError){0} };\n",
        "}\n",
        "static int64_t ku_pg_rows(PGresult* r) { return r ? (int64_t)PQntuples(r) : 0; }\n",
        "static int64_t ku_pg_cols(PGresult* r) { return r ? (int64_t)PQnfields(r) : 0; }\n",
        "static KuString ku_pg_value(PGresult* r, int64_t row, int64_t col) {\n",
        "  if (!r) return (KuString){0};\n",
        "  return ku_pg_copy_cstr(PQgetvalue(r, (int)row, (int)col));\n",
        "}\n",
        // Returns a null value (uint8_t) so it can materialize into a `null` temp.
        "static uint8_t ku_pg_close(PGconn* conn) { if (conn) PQfinish(conn); return 0; }\n",
    ));
    // Connection pool — bounded, thread-safe (CRITICAL_SECTION + CONDITION_VARIABLE),
    // leak-proof by construction: the pool owns every connection, queries borrow one
    // and always return it, and `pool_close`/drop finishes them all. A caller never
    // holds a raw connection, so none can be leaked or double-closed.
    if program_uses_pg_pool(program) {
        out.push_str(concat!(
            "struct KuPgPool { PGconn** conns; char* in_use; size_t size; char* conninfo; CRITICAL_SECTION lock; CONDITION_VARIABLE cv; };\n",
            "static KuResult_pg_pool ku_pg_pool(KuString conninfo, int64_t size) {\n",
            "  if (size <= 0) return (KuResult_pg_pool){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"pool_error\", 10), ku_string_static((const uint8_t*)\"pool size must be > 0\", 21)) };\n",
            "  KuPgPool* p = (KuPgPool*)malloc(sizeof(KuPgPool));\n",
            "  if (!p) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
            "  p->size = (size_t)size;\n",
            "  p->conns = (PGconn**)calloc(p->size, sizeof(PGconn*));\n",
            "  p->in_use = (char*)calloc(p->size, 1);\n",
            "  p->conninfo = ku_string_to_cstr(conninfo);\n",
            "  if (!p->conns || !p->in_use || !p->conninfo) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n",
            "  InitializeCriticalSection(&p->lock); InitializeConditionVariable(&p->cv);\n",
            "  return (KuResult_pg_pool){ true, p, (KuError){0} };\n",
            "}\n",
            // Acquire a slot (blocking). Returns slot index and sets *out; on connect
            // failure returns -1 with *err set. A reserved-but-uncreated slot has
            // conns[i]==NULL && in_use[i]==1, so no two threads pick the same slot.
            "static int ku_pg_pool_acquire(KuPgPool* p, PGconn** out, KuString* err) {\n",
            "  EnterCriticalSection(&p->lock);\n",
            "  for (;;) {\n",
            "    for (size_t i = 0; i < p->size; i++) if (p->conns[i] && !p->in_use[i]) { p->in_use[i] = 1; *out = p->conns[i]; LeaveCriticalSection(&p->lock); return (int)i; }\n",
            "    int made = -1;\n",
            "    for (size_t i = 0; i < p->size; i++) if (!p->conns[i] && !p->in_use[i]) { p->in_use[i] = 1; made = (int)i; break; }\n",
            "    if (made >= 0) {\n",
            "      LeaveCriticalSection(&p->lock);\n",
            "      PGconn* h = PQconnectdb(p->conninfo);\n",
            "      if (!h || PQstatus(h) != 0) {\n",
            "        KuString e = ku_pg_copy_cstr(h ? PQerrorMessage(h) : \"connect failed\"); if (h) PQfinish(h);\n",
            "        EnterCriticalSection(&p->lock); p->in_use[made] = 0; WakeConditionVariable(&p->cv); LeaveCriticalSection(&p->lock);\n",
            "        *err = e; return -1;\n",
            "      }\n",
            "      EnterCriticalSection(&p->lock); p->conns[made] = h; LeaveCriticalSection(&p->lock);\n",
            "      *out = h; return made;\n",
            "    }\n",
            "    SleepConditionVariableCS(&p->cv, &p->lock, INFINITE);\n",
            "  }\n",
            "}\n",
            "static void ku_pg_pool_release(KuPgPool* p, int slot, int broken) {\n",
            "  EnterCriticalSection(&p->lock);\n",
            "  if (broken && p->conns[slot]) { PQfinish(p->conns[slot]); p->conns[slot] = 0; }\n",
            "  p->in_use[slot] = 0; WakeConditionVariable(&p->cv); LeaveCriticalSection(&p->lock);\n",
            "}\n",
            "static KuResult_pg_result ku_pg_pool_query(KuPgPool* p, KuString sql) {\n",
            "  PGconn* c = 0; KuString err = (KuString){0};\n",
            "  int slot = ku_pg_pool_acquire(p, &c, &err);\n",
            "  if (slot < 0) return (KuResult_pg_result){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"pool_error\", 10), err) };\n",
            "  KuResult_pg_result r = ku_pg_query(c, sql);\n",
            "  ku_pg_pool_release(p, slot, PQstatus(c) != 0);\n",
            "  return r;\n",
            "}\n",
            "static KuResult_pg_result ku_pg_pool_query_params(KuPgPool* p, KuString sql, KuArray_str params) {\n",
            "  PGconn* c = 0; KuString err = (KuString){0};\n",
            "  int slot = ku_pg_pool_acquire(p, &c, &err);\n",
            "  if (slot < 0) return (KuResult_pg_result){ false, 0, ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"pool_error\", 10), err) };\n",
            "  KuResult_pg_result r = ku_pg_query_params(c, sql, params);\n",
            "  ku_pg_pool_release(p, slot, PQstatus(c) != 0);\n",
            "  return r;\n",
            "}\n",
            "static void ku_pg_pool_free(KuPgPool* p) {\n",
            "  if (!p) return;\n",
            "  for (size_t i = 0; i < p->size; i++) if (p->conns[i]) PQfinish(p->conns[i]);\n",
            "  DeleteCriticalSection(&p->lock); free(p->conns); free(p->in_use); free(p->conninfo); free(p);\n",
            "}\n",
            "static KuPgPool* ku_move_pg_pool(KuPgPool** p) { KuPgPool* m = *p; *p = 0; return m; }\n",
            "static void ku_drop_pg_pool(KuPgPool** p) { if (p && *p) { ku_pg_pool_free(*p); *p = 0; } }\n",
            "static KuPgPool* ku_clone_pg_pool(KuPgPool* c) { (void)c; fprintf(stderr, \"cannot clone a pg pool\\n\"); exit(1); }\n",
            "static uint8_t ku_pg_pool_close(KuPgPool* p) { ku_pg_pool_free(p); return 0; }\n",
        ));
    }
    out.push('\n');
}

fn ir_type_uses_http(ty: &IrType) -> bool {
    match ty {
        IrType::Named(name) => name.starts_with("__ku_http_"),
        IrType::Array(inner) | IrType::Result(inner) | IrType::Cell(inner) => {
            ir_type_uses_http(inner)
        }
        IrType::Closure { params, ret } => {
            params.iter().any(ir_type_uses_http) || ir_type_uses_http(ret)
        }
        _ => false,
    }
}

fn inst_uses_http(inst: &IrInst) -> bool {
    match inst {
        IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
            ir_type_uses_http(ty) || ir_type_uses_http(&value.ty)
        }
        IrInst::BindOk { ty, result, .. } => {
            ir_type_uses_http(ty) || ir_type_uses_http(&result.ty)
        }
        IrInst::Store { value, .. }
        | IrInst::Print(value)
        | IrInst::Expr(value)
        | IrInst::Fail(value)
        | IrInst::Panic(value) => ir_type_uses_http(&value.ty),
        _ => false,
    }
}

/// Stage 8a: the response/request structs plus their deep clone/drop/move
/// helpers (named to match the generic `c_named_*_function` dispatch so the
/// Result ABI and value-ownership paths pick them up automatically). Emitted
/// before the Result ABI so `KuResult_struct___ku_http_response` can embed the
/// response struct.
fn emit_http_types(out: &mut String, program: &IrProgram) -> KuResult<()> {
    if !program_uses_http(program) {
        return Ok(());
    }
    out.push_str(
        "typedef struct { int64_t status; KuString content_type; KuString body; KuString location; } KuStruct___ku_http_response;\n\
         static KuStruct___ku_http_response ku_move_struct___ku_http_response(KuStruct___ku_http_response* v) { KuStruct___ku_http_response r = *v; *v = (KuStruct___ku_http_response){0}; return r; }\n\
         static KuStruct___ku_http_response ku_clone_struct___ku_http_response(KuStruct___ku_http_response v) { v.content_type = ku_string_clone(v.content_type); v.body = ku_string_clone(v.body); v.location = ku_string_clone(v.location); return v; }\n\
         static void ku_drop_struct___ku_http_response(KuStruct___ku_http_response* v) { if (!v) return; ku_string_drop(&v->content_type); ku_string_drop(&v->body); ku_string_drop(&v->location); *v = (KuStruct___ku_http_response){0}; }\n\
         typedef struct { KuString method; KuString path; KuString body; KuObject* params; KuObject* query; KuObject* headers; } KuStruct___ku_http_request;\n\
         static KuStruct___ku_http_request ku_move_struct___ku_http_request(KuStruct___ku_http_request* v) { KuStruct___ku_http_request r = *v; *v = (KuStruct___ku_http_request){0}; return r; }\n\
         static KuStruct___ku_http_request ku_clone_struct___ku_http_request(KuStruct___ku_http_request v) { v.method = ku_string_clone(v.method); v.path = ku_string_clone(v.path); v.body = ku_string_clone(v.body); v.params = ku_object_clone(v.params); v.query = ku_object_clone(v.query); v.headers = ku_object_clone(v.headers); return v; }\n\
         static void ku_drop_struct___ku_http_request(KuStruct___ku_http_request* v) { if (!v) return; ku_string_drop(&v->method); ku_string_drop(&v->path); ku_string_drop(&v->body); ku_object_drop(v->params); ku_object_drop(v->query); ku_object_drop(v->headers); *v = (KuStruct___ku_http_request){0}; }\n\n",
    );
    Ok(())
}

/// Stage 8a: the single-threaded winsock server runtime. Emitted after the
/// Result ABI (it calls handlers whose return is `KuResult_struct___ku_http_response`)
/// and after `KuEnvHeader` (route env release). Uppercase method + exact-path
/// (query-stripped, segment-normalized) routing, 404/405 fallbacks matching the
/// interpreter. `KU_HTTP_MAX_REQUESTS` (env) bounds the loop for leak/ASan runs.
fn emit_http_runtime(out: &mut String, program: &IrProgram) -> KuResult<()> {
    if !program_uses_http(program) {
        return Ok(());
    }
    out.push_str(
        r####"#define KU_HTTP_MAX_SEGS 64
/* Longest accepted request target, pinned to the interpreter's
   MAX_REQUEST_TARGET_BYTES (src/stdlib/http.rs). A longer target is answered with
   414 before it is copied or routed -- never truncated: truncating would let two
   distinct paths that share a long prefix resolve to the same route. */
#define KU_HTTP_MAX_TARGET 8192
/* A terminal handler registered at a trie node, one per HTTP method. */
typedef struct { char* method; void* invoke; void* env; int arity; int returns_result; } KuHttpHandler;
/* Stage 8b: a routing trie node. Each node holds static children (keyed by the
   literal path segment) and at most one `{param}` child. Matching prefers a
   static child over the param child at every segment (with backtracking), which
   mirrors the interpreter's exact-shape-before-param-scan lookup. */
typedef struct KuHttpNode {
  char* seg;                     /* static segment label for this node (NULL at root) */
  struct KuHttpNode** children;  /* static children */
  size_t nchild; size_t cchild;
  struct KuHttpNode* param;      /* single `{param}` child, or NULL */
  char* param_name;              /* name bound when descending into `param` */
  KuHttpHandler* handlers;       /* terminal handlers, keyed by method */
  size_t nh; size_t ch;
} KuHttpNode;
/* Stage 8d/8e: the route trie plus the limits read from the `http.server({...})`
   config object (or assigned field-by-field). Defaults mirror the interpreter's
   HttpServerRuntimeLimits (src/stdlib/http.rs:16-25).

   These are `long long`, not `long`: the interpreter stores every limit as i64,
   and `long` is 32-bit on Windows LLP64, which would silently truncate a limit
   above 2^31 (e.g. `max_body_bytes: 3_000_000_000` wrapping negative and
   disabling the 413 check). */
typedef struct {
  KuHttpNode* root;
  long long max_connections;
  long long max_active_requests;
  long long max_pending_requests;
  long long handler_timeout_ms;
  long long max_body_bytes;
  long long max_header_bytes;
  long long read_header_timeout_ms;
  long long read_body_timeout_ms;
  long long write_timeout_ms;
  long long idle_timeout_ms;
} KuHttpServer;
static void ku_http_normalize_path(const char* in, size_t in_len, char* out, size_t out_cap) {
  size_t oi = 0; size_t i = 0;
  if (out_cap == 0) return;
  out[oi++] = '/';
  while (i < in_len) {
    while (i < in_len && in[i] == '/') i++;
    size_t start = i;
    while (i < in_len && in[i] != '/') i++;
    if (i > start) {
      if (oi > 1 && oi + 1 < out_cap) out[oi++] = '/';
      size_t seg = i - start;
      if (oi + seg >= out_cap) seg = out_cap - 1 - oi;
      memcpy(out + oi, in + start, seg); oi += seg;
    }
  }
  out[oi] = '\0';
}
static KuHttpNode* ku_http_node_new(void) {
  KuHttpNode* n = (KuHttpNode*)calloc(1, sizeof(KuHttpNode));
  if (!n) { fprintf(stderr, "out of memory\n"); exit(1); }
  return n;
}
static KuHttpServer* ku_http_server_new(void) {
  KuHttpServer* s = (KuHttpServer*)calloc(1, sizeof(KuHttpServer));
  if (!s) { fprintf(stderr, "out of memory\n"); exit(1); }
  s->root = ku_http_node_new();
  s->max_connections = 1024;
  s->max_active_requests = 256;
  s->max_pending_requests = 1024;
  s->handler_timeout_ms = 15000;
  s->max_body_bytes = 1000000;
  s->max_header_bytes = 16 * 1024;
  s->read_header_timeout_ms = 5000;
  s->read_body_timeout_ms = 10000;
  s->write_timeout_ms = 10000;
  s->idle_timeout_ms = 5000;
  return s;
}
/* Spell a KuValue tag the way the interpreter's Value::type_name() does, so a
   wrong-typed config field reports the same text on both runtimes. */
static const char* ku_http_value_type_name(KuValueTag tag) {
  switch (tag) {
    case KU_INT: return "int";
    case KU_BOOL: return "bool";
    case KU_STR: return "str";
    case KU_OBJECT: return "object";
    case KU_ARRAY: return "array";
    case KU_FUNCTION: return "function";
    default: return "null";
  }
}
/* Read an int field from the `http.server({...})` config object. Mirrors the
   interpreter's optional_int (src/stdlib/http.rs:418) exactly: absent or null
   yields the default, a positive int yields that value, and a non-positive int
   or a wrong-typed value is a hard error carrying the interpreter's message.
   Silently defaulting instead (the previous behaviour) would let native accept
   configs the interpreter rejects. `http.server(...)` has no Result channel in
   native, so this reports and exits like the runtime's other faults.
   The config object is only READ here; it stays owned by the caller, which
   drops it after `http.server(...)` returns. */
static long long ku_http_cfg_int(KuObject* config, const char* key, long long dflt) {
  if (!config) return dflt;
  KuValue* v = ku_object_get(config, ku_string_static((const uint8_t*)key, strlen(key)));
  if (!v || v->tag == KU_NULL) return dflt;
  if (v->tag != KU_INT) {
    fprintf(stderr, "type error: http.request field '%s' must be int but got %s\n", key, ku_http_value_type_name(v->tag));
    exit(1);
  }
  if (v->as.i <= 0) {
    fprintf(stderr, "http config field '%s' must be a positive int\n", key);
    exit(1);
  }
  return (long long)v->as.i;
}
/* `http.server(config)` / `http.service(config)`: start from the defaults, then
   override each admission-control limit present in the config object. */
static KuHttpServer* ku_http_server_new_cfg(KuObject* config) {
  KuHttpServer* s = ku_http_server_new();
  s->max_connections = ku_http_cfg_int(config, "max_connections", s->max_connections);
  s->max_active_requests = ku_http_cfg_int(config, "max_active_requests", s->max_active_requests);
  s->max_pending_requests = ku_http_cfg_int(config, "max_pending_requests", s->max_pending_requests);
  s->handler_timeout_ms = ku_http_cfg_int(config, "handler_timeout_ms", s->handler_timeout_ms);
  s->max_body_bytes = ku_http_cfg_int(config, "max_body_bytes", s->max_body_bytes);
  s->max_header_bytes = ku_http_cfg_int(config, "max_header_bytes", s->max_header_bytes);
  s->read_header_timeout_ms = ku_http_cfg_int(config, "read_header_timeout_ms", s->read_header_timeout_ms);
  s->read_body_timeout_ms = ku_http_cfg_int(config, "read_body_timeout_ms", s->read_body_timeout_ms);
  s->write_timeout_ms = ku_http_cfg_int(config, "write_timeout_ms", s->write_timeout_ms);
  s->idle_timeout_ms = ku_http_cfg_int(config, "idle_timeout_ms", s->idle_timeout_ms);
  return s;
}
static KuHttpNode* ku_http_node_child(KuHttpNode* node, const char* seg, size_t seg_len) {
  for (size_t i = 0; i < node->nchild; i++) {
    KuHttpNode* c = node->children[i];
    if (strlen(c->seg) == seg_len && memcmp(c->seg, seg, seg_len) == 0) return c;
  }
  return NULL;
}
static KuHttpNode* ku_http_node_add_static(KuHttpNode* node, const char* seg, size_t seg_len) {
  KuHttpNode* existing = ku_http_node_child(node, seg, seg_len);
  if (existing) return existing;
  if (node->nchild + 1 > node->cchild) {
    size_t nc = node->cchild ? node->cchild * 2 : 4;
    node->children = (KuHttpNode**)realloc(node->children, nc * sizeof(KuHttpNode*));
    if (!node->children) { fprintf(stderr, "out of memory\n"); exit(1); }
    node->cchild = nc;
  }
  KuHttpNode* child = ku_http_node_new();
  child->seg = (char*)malloc(seg_len + 1);
  if (!child->seg) { fprintf(stderr, "out of memory\n"); exit(1); }
  memcpy(child->seg, seg, seg_len); child->seg[seg_len] = '\0';
  node->children[node->nchild++] = child;
  return child;
}
static KuHttpNode* ku_http_node_add_param(KuHttpNode* node, const char* name, size_t name_len) {
  if (!node->param) {
    node->param = ku_http_node_new();
    node->param_name = (char*)malloc(name_len + 1);
    if (!node->param_name) { fprintf(stderr, "out of memory\n"); exit(1); }
    memcpy(node->param_name, name, name_len); node->param_name[name_len] = '\0';
  }
  return node->param;
}
static KuHttpServer* ku_http_server_add_route(KuHttpServer* s, KuString method, KuString path, void* invoke, void* env, int arity, int returns_result) {
  /* Normalize into a buffer sized to the actual path, never a fixed one. A fixed
     buffer truncated a long registered path down to a SHORTER route that ordinary
     requests could then match -- and it made two routes sharing a long prefix
     collapse onto the same trie path. The interpreter puts no length limit on a
     registered route, so neither does this. */
  size_t pcap = path.len + 2;
  char* pbuf = (char*)malloc(pcap);
  if (!pbuf) { fprintf(stderr, "out of memory\n"); exit(1); }
  ku_http_normalize_path((const char*)path.ptr, path.len, pbuf, pcap);
  size_t plen = strlen(pbuf);
  KuHttpNode* node = s->root;
  size_t i = 0;
  while (i < plen) {
    while (i < plen && pbuf[i] == '/') i++;
    size_t start = i;
    while (i < plen && pbuf[i] != '/') i++;
    size_t seg_len = i - start;
    if (seg_len == 0) continue;
    const char* seg = pbuf + start;
    if (seg_len >= 2 && seg[0] == '{' && seg[seg_len - 1] == '}') {
      node = ku_http_node_add_param(node, seg + 1, seg_len - 2);
    } else {
      node = ku_http_node_add_static(node, seg, seg_len);
    }
  }
  if (node->nh + 1 > node->ch) {
    size_t nc = node->ch ? node->ch * 2 : 2;
    node->handlers = (KuHttpHandler*)realloc(node->handlers, nc * sizeof(KuHttpHandler));
    if (!node->handlers) { fprintf(stderr, "out of memory\n"); exit(1); }
    node->ch = nc;
  }
  KuHttpHandler* h = &node->handlers[node->nh++];
  h->method = ku_string_to_cstr(method);
  h->invoke = invoke; h->env = env; h->arity = arity; h->returns_result = returns_result;
  ku_string_drop(&method); ku_string_drop(&path);
  free(pbuf);
  return s;
}
static KuHttpHandler* ku_http_node_handler(KuHttpNode* node, const char* method) {
  for (size_t i = 0; i < node->nh; i++)
    if (strcmp(node->handlers[i].method, method) == 0) return &node->handlers[i];
  return NULL;
}
static void ku_http_node_free(KuHttpNode* node) {
  if (!node) return;
  for (size_t i = 0; i < node->nchild; i++) ku_http_node_free(node->children[i]);
  free(node->children);
  ku_http_node_free(node->param);
  free(node->param_name);
  free(node->seg);
  for (size_t i = 0; i < node->nh; i++) {
    free(node->handlers[i].method);
    if (node->handlers[i].env) ((KuEnvHeader*)node->handlers[i].env)->release(node->handlers[i].env);
  }
  free(node->handlers);
  free(node);
}
static void ku_http_server_free(KuHttpServer* s) {
  if (!s) return;
  ku_http_node_free(s->root);
  free(s);
}
static const char* ku_http_status_text(int64_t status) {
  switch (status) {
    case 200: return "OK"; case 201: return "Created"; case 202: return "Accepted"; case 204: return "No Content";
    case 301: return "Moved Permanently"; case 302: return "Found"; case 303: return "See Other"; case 304: return "Not Modified";
    case 307: return "Temporary Redirect"; case 308: return "Permanent Redirect";
    case 400: return "Bad Request"; case 401: return "Unauthorized"; case 403: return "Forbidden"; case 404: return "Not Found";
    case 405: return "Method Not Allowed"; case 408: return "Request Timeout"; case 413: return "Content Too Large";
    case 414: return "URI Too Long";
    case 431: return "Request Header Fields Too Large"; case 500: return "Internal Server Error";
    case 501: return "Not Implemented"; case 502: return "Bad Gateway"; case 503: return "Service Unavailable"; case 504: return "Gateway Timeout";
    default: return "OK";
  }
}
static KuString ku_http_string_copy(const char* ptr, size_t len) {
  if (len == 0) return (KuString){0};
  uint8_t* data = (uint8_t*)malloc(len);
  if (!data) { fprintf(stderr, "out of memory\n"); exit(1); }
  memcpy(data, ptr, len);
  return (KuString){ data, len, len, KU_STRING_OWNED };
}
static KuStruct___ku_http_response ku_http_response_from_result(KuResult_struct___ku_http_response r) {
  if (r.ok) { KuStruct___ku_http_response v = r.value; r.value = (KuStruct___ku_http_response){0}; return v; }
  KuStruct___ku_http_response v;
  v.status = 500;
  v.content_type = ku_string_static((const uint8_t*)"text/plain; charset=utf-8", 25);
  v.body = ku_string_clone(r.error.message);
  v.location = (KuString){0};
  ku_error_drop(&r.error);
  return v;
}
static void ku_http_send_all(SOCKET cli, const char* data, size_t len) {
  size_t sent = 0;
  while (sent < len) { int n = send(cli, data + sent, (int)(len - sent), 0); if (n <= 0) break; sent += (size_t)n; }
}
/* Send a snprintf-formatted line, clamped to what was actually written.
   snprintf returns the length the output WOULD have had, so passing that return
   value straight to a send() length reads past the end of the buffer. Callers
   here only format bounded numeric lines, but the clamp keeps that guarantee
   local instead of relying on every future caller re-deriving it. */
static void ku_http_send_fmt(SOCKET cli, const char* buf, size_t cap, int written) {
  if (written <= 0) return;
  size_t len = (size_t)written;
  if (len > cap - 1) len = cap - 1;
  ku_http_send_all(cli, buf, len);
}
static void ku_http_write_response(SOCKET cli, KuStruct___ku_http_response* resp) {
  char head[256];
  int hn = snprintf(head, sizeof(head), "HTTP/1.1 %lld %s\r\n", (long long)resp->status, ku_http_status_text(resp->status));
  ku_http_send_fmt(cli, head, sizeof(head), hn);
  /* Lowercase header names match the interpreter's wire format (it stores
     headers in a lowercase-keyed map). Header *order* cannot match byte-for-byte
     because the interpreter iterates a HashMap (non-deterministic order); native
     emits a fixed, deterministic order, which is semantically identical (HTTP
     header names are case-insensitive and order-independent).

     Header VALUES are handler-controlled and unbounded (e.g. `http.redirect` of a
     target built from `req.query`), so they are streamed straight from the
     KuString instead of being formatted through a fixed stack buffer: the
     interpreter builds an unbounded String and never truncates, and formatting a
     >buffer value here would both truncate and (via snprintf's would-be return)
     send adjacent stack memory to the client. */
  if (resp->content_type.len) {
    ku_http_send_all(cli, "content-type: ", 14);
    ku_http_send_all(cli, (const char*)resp->content_type.ptr, resp->content_type.len);
    ku_http_send_all(cli, "\r\n", 2);
  }
  if (resp->location.len) {
    ku_http_send_all(cli, "location: ", 10);
    ku_http_send_all(cli, (const char*)resp->location.ptr, resp->location.len);
    ku_http_send_all(cli, "\r\n", 2);
  }
  int cl = snprintf(head, sizeof(head), "content-length: %llu\r\nconnection: close\r\n\r\n", (unsigned long long)resp->body.len);
  ku_http_send_fmt(cli, head, sizeof(head), cl);
  if (resp->body.len) ku_http_send_all(cli, (const char*)resp->body.ptr, resp->body.len);
}
static void ku_http_write_status(SOCKET cli, int64_t status, const char* message) {
  KuStruct___ku_http_response r;
  r.status = status;
  r.content_type = ku_string_static((const uint8_t*)"text/plain; charset=utf-8", 25);
  r.body = ku_string_static((const uint8_t*)message, strlen(message));
  r.location = (KuString){0};
  ku_http_write_response(cli, &r);
}
/* `req.params`/`req.query`/`req.headers` `.field` read: an owned KuString copy of
   the string value, or an empty string when the key is absent (the interpreter's
   StringMap `.field` yields str; absent-key aborts there but is never exercised
   by the routing tests, so native returns "" instead of crashing). */
static KuString ku_http_map_get(KuObject* o, KuString key) {
  KuValue* v = ku_object_get(o, key);
  if (v && v->tag == KU_STR) return ku_string_clone(v->as.s);
  return ku_string_static((const uint8_t*)"", 0);
}
/* `?k=v&k2=v2` -> KuObject{str->str}. Empty parts and empty keys are skipped;
   a key with no `=` maps to "" (matches the interpreter's split_path_query). */
static KuObject* ku_http_parse_query(const char* target, size_t path_len) {
  KuObject* q = ku_object_new(0);
  const char* p = target + path_len;
  if (*p != '?') return q;
  p++;
  while (*p) {
    const char* amp = p; while (*amp && *amp != '&') amp++;
    const char* eq = p; while (eq < amp && *eq != '=') eq++;
    size_t klen = (size_t)(eq - p);
    const char* vstart; size_t vlen;
    if (eq < amp) { vstart = eq + 1; vlen = (size_t)(amp - (eq + 1)); }
    else { vstart = amp; vlen = 0; }
    if (klen > 0) {
      KuString key = ku_http_string_copy(p, klen);
      KuString val = ku_http_string_copy(vstart, vlen);
      ku_object_set(q, key, ku_v_str(val));
    }
    if (*amp == '&') p = amp + 1; else break;
  }
  return q;
}
/* Request header lines -> KuObject{lowercased-name -> trimmed-value}, matching the
   interpreter (header names lowercased, names and values trimmed). */
static KuObject* ku_http_parse_headers(const char* buf, int header_end) {
  KuObject* h = ku_object_new(0);
  int i = 0;
  while (i + 1 < header_end && !(buf[i] == '\r' && buf[i + 1] == '\n')) i++;
  if (i + 1 < header_end) i += 2; else return h;
  while (i < header_end) {
    int ls = i;
    while (i + 1 < header_end && !(buf[i] == '\r' && buf[i + 1] == '\n')) i++;
    int le = (i + 1 < header_end) ? i : header_end;
    if (i + 1 < header_end) i += 2; else i = header_end;
    if (le <= ls) continue;
    int colon = ls; while (colon < le && buf[colon] != ':') colon++;
    if (colon >= le) continue;
    int ns = ls, ne = colon;
    while (ns < ne && (buf[ns] == ' ' || buf[ns] == '\t')) ns++;
    while (ne > ns && (buf[ne - 1] == ' ' || buf[ne - 1] == '\t')) ne--;
    int vs = colon + 1, ve = le;
    while (vs < ve && (buf[vs] == ' ' || buf[vs] == '\t')) vs++;
    while (ve > vs && (buf[ve - 1] == ' ' || buf[ve - 1] == '\t')) ve--;
    size_t nlen = (size_t)(ne - ns);
    if (nlen == 0) continue;
    KuString key = ku_http_string_copy(buf + ns, nlen);
    for (size_t k = 0; k < key.len; k++) { uint8_t c = ((uint8_t*)key.ptr)[k]; if (c >= 'A' && c <= 'Z') ((uint8_t*)key.ptr)[k] = (uint8_t)(c + 32); }
    KuString val = ku_http_string_copy(buf + vs, (size_t)(ve - vs));
    ku_object_set(h, key, ku_v_str(val));
  }
  return h;
}
/* Trie match with static-before-param priority and backtracking. Returns the
   handler registered for `method` at the terminal node reached by consuming every
   request segment, binding `{param}` segments into `params` on the matched path.
   The static-first-with-backtracking walk reproduces the interpreter's
   exact-shape-then-param-scan result (e.g. `/user/me` beats `/user/{id}`). */
static KuHttpHandler* ku_http_trie_match(KuHttpNode* node, const char** segs, size_t* seglens, size_t nseg, size_t i, const char* method, KuObject* params) {
  if (i == nseg) return ku_http_node_handler(node, method);
  KuHttpNode* sc = ku_http_node_child(node, segs[i], seglens[i]);
  if (sc) {
    KuHttpHandler* r = ku_http_trie_match(sc, segs, seglens, nseg, i + 1, method, params);
    if (r) return r;
  }
  if (node->param) {
    KuHttpHandler* r = ku_http_trie_match(node->param, segs, seglens, nseg, i + 1, method, params);
    if (r) {
      KuString key = ku_http_string_copy(node->param_name, strlen(node->param_name));
      KuString val = ku_http_string_copy(segs[i], seglens[i]);
      ku_object_set(params, key, ku_v_str(val));
      return r;
    }
  }
  return NULL;
}
/* True when the path matches some route under any method (405-vs-404 decision). */
static int ku_http_trie_path_exists(KuHttpNode* node, const char** segs, size_t* seglens, size_t nseg, size_t i) {
  if (i == nseg) return node->nh > 0 ? 1 : 0;
  KuHttpNode* sc = ku_http_node_child(node, segs[i], seglens[i]);
  if (sc && ku_http_trie_path_exists(sc, segs, seglens, nseg, i + 1)) return 1;
  if (node->param && ku_http_trie_path_exists(node->param, segs, seglens, nseg, i + 1)) return 1;
  return 0;
}
/* The ASCII subset of Rust's `char::is_whitespace`, which is what the
   interpreter's `first_line.split_whitespace()` uses to tokenize the request
   line. Splitting on space/tab alone would accept request lines the interpreter
   rejects (its tokenizer also breaks on CR, LF, VT and FF). */
static int ku_http_is_ws(unsigned char c) {
  return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == 0x0b || c == 0x0c;
}
/* Stage 8e: read and validate one request under the same request-level limits the
   interpreter enforces (read_http_request / read_http_header). Header bytes are
   read one at a time into a growable buffer (bounded by max_header_bytes) so the
   \r\n\r\n boundary, the 431 cutoff, and the idle->read-header timeout hand-off all
   match the interpreter exactly; body bytes are read under read_body_timeout. On
   any wire/limit violation this answers with the interpreter's status (400/408/
   413/431) and returns. Every malloc'd buffer is freed on every path. */
static void ku_http_handle_connection(KuHttpServer* server, SOCKET cli) {
  long long max_header = server->max_header_bytes > 0 ? server->max_header_bytes : (16 * 1024);
  long long max_body = server->max_body_bytes >= 0 ? server->max_body_bytes : 1000000;
  DWORD idle_tmo = (DWORD)(server->idle_timeout_ms > 0 ? server->idle_timeout_ms : 5000);
  DWORD hdr_tmo = (DWORD)(server->read_header_timeout_ms > 0 ? server->read_header_timeout_ms : 5000);
  DWORD body_tmo = (DWORD)(server->read_body_timeout_ms > 0 ? server->read_body_timeout_ms : 10000);
  /* write_timeout_ms applies to every response write on this connection. The
     interpreter calls set_write_timeout before each write_http_response; setting
     SO_SNDTIMEO once per connection covers the same sends. Without this the
     config field was read and stored but never had any effect, so a wedged peer
     could block a worker in send() forever. */
  { DWORD t = (DWORD)(server->write_timeout_ms > 0 ? server->write_timeout_ms : 10000);
    setsockopt(cli, SOL_SOCKET, SO_SNDTIMEO, (const char*)&t, sizeof(t)); }
  /* Header read: one byte at a time into a growable buffer. First byte waits up to
     idle_timeout; subsequent header bytes wait up to read_header_timeout. recv
     error/timeout (n<0) before the header completes -> 408; peer close (n==0)
     before the header completes -> 400; exceeding max_header_bytes -> 431. */
  size_t cap = 1024, hlen = 0;
  char* hdr = (char*)malloc(cap);
  if (!hdr) { ku_http_write_status(cli, 500, "Internal Server Error"); return; }
  { DWORD t = idle_tmo; setsockopt(cli, SOL_SOCKET, SO_RCVTIMEO, (const char*)&t, sizeof(t)); }
  int header_done = 0;
  for (;;) {
    char c;
    int n = recv(cli, &c, 1, 0);
    if (n == 0) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
    if (n < 0) { free(hdr); ku_http_write_status(cli, 408, "Request Timeout"); return; }
    if (hlen + 1 > cap) {
      cap *= 2;
      char* nb = (char*)realloc(hdr, cap);
      if (!nb) { free(hdr); ku_http_write_status(cli, 500, "Internal Server Error"); return; }
      hdr = nb;
    }
    hdr[hlen++] = c;
    if (hlen == 1) { DWORD t = hdr_tmo; setsockopt(cli, SOL_SOCKET, SO_RCVTIMEO, (const char*)&t, sizeof(t)); }
    if ((long long)hlen > max_header) { free(hdr); ku_http_write_status(cli, 431, "Request Header Fields Too Large"); return; }
    if (hlen >= 4 && hdr[hlen-4] == '\r' && hdr[hlen-3] == '\n' && hdr[hlen-2] == '\r' && hdr[hlen-1] == '\n') { hlen -= 4; header_done = 1; break; }
    if (hlen >= 2 && hdr[hlen-2] == '\n' && hdr[hlen-1] == '\n') { hlen -= 2; header_done = 1; break; }
  }
  if (!header_done || hlen == 0) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  /* Split the header text on "\r\n" ONLY, exactly like the interpreter's
     `header_text.split("\r\n")` -- even though the read above also accepts a bare
     "\n\n" terminator (read_http_header does too). This pairing is what makes an
     LF-only request a 400 on both runtimes: with no CRLF anywhere, the entire
     header collapses into the "first line", whose tokenization then yields more
     than 3 tokens. Ending the first line at a lone '\n' instead would make native
     cheerfully serve LF-only requests that the interpreter rejects. */
  size_t fl_end = hlen;
  for (size_t i = 0; i + 1 < hlen; i++) { if (hdr[i] == '\r' && hdr[i + 1] == '\n') { fl_end = i; break; } }
  /* The request line must tokenize to exactly 3 parts (method target version). */
  size_t tok_start[4] = {0}; size_t tok_len[4] = {0}; int ntok = 0;
  { size_t p = 0;
    while (p < fl_end) {
      while (p < fl_end && ku_http_is_ws((unsigned char)hdr[p])) p++;
      if (p >= fl_end) break;
      size_t st = p;
      while (p < fl_end && !ku_http_is_ws((unsigned char)hdr[p])) p++;
      if (ntok < 4) { tok_start[ntok] = st; tok_len[ntok] = p - st; }
      ntok++;
    }
  }
  if (ntok != 3) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  /* Every remaining "\r\n"-separated line must contain ':'; empty lines are
     skipped (the interpreter's `line.split_once(':')` returning None is a 400). */
  { size_t i = (fl_end < hlen) ? fl_end + 2 : hlen;
    while (i < hlen) {
      size_t ls = i;
      size_t le = hlen;
      for (size_t k = i; k + 1 < hlen; k++) { if (hdr[k] == '\r' && hdr[k + 1] == '\n') { le = k; break; } }
      if (le > ls) {
        int has_colon = 0;
        for (size_t k = ls; k < le; k++) if (hdr[k] == ':') { has_colon = 1; break; }
        if (!has_colon) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
      }
      if (le >= hlen) break;
      i = le + 2;
    }
  }
  char method[16] = {0};
  { size_t ml = tok_len[0]; if (ml > 15) ml = 15; memcpy(method, hdr + tok_start[0], ml); method[ml] = '\0';
    for (size_t k = 0; method[k]; k++) if (method[k] >= 'a' && method[k] <= 'z') method[k] = (char)(method[k] - 32); }
  /* The target is length-checked BEFORE it is copied and before any routing, so an
     over-long target can never be truncated into a shorter one that matches a real
     route, and never reaches a handler. */
  if (tok_len[1] > (size_t)KU_HTTP_MAX_TARGET) { free(hdr); ku_http_write_status(cli, 414, "URI Too Long"); return; }
  char target[KU_HTTP_MAX_TARGET + 1] = {0};
  { size_t tl = tok_len[1]; memcpy(target, hdr + tok_start[1], tl); target[tl] = '\0'; }
  /* Header map (lowercased names, trimmed values) matching the interpreter, plus a
     strict content-length parse: present-but-non-numeric -> 400. */
  KuObject* headers = ku_http_parse_headers(hdr, (int)hlen);
  /* content-length is parsed exactly like the interpreter's `value.parse::<usize>()`
     in read_http_request: an unsigned 64-bit parse that accepts one leading '+'
     (Rust's integer FromStr does), where anything that is not a plain number --
     a non-digit, an empty or lone-sign string, or a value too large for usize --
     is a parse failure and therefore 400.

     The accumulator is unsigned WITH an explicit overflow guard. A signed
     accumulator wraps on a huge value (undefined behaviour), and the wrapped
     negative result would then slip past the 413 check and reach the handler with
     an empty body and a 200 -- where the interpreter answers 400. */
  unsigned long long content_length = 0;
  { KuValue* cv = ku_object_get(headers, ku_string_static((const uint8_t*)"content-length", 14));
    if (cv && cv->tag == KU_STR) {
      KuString s = cv->as.s;
      unsigned long long v = 0; int ok = 1; size_t k = 0;
      if (s.len > 0 && ((uint8_t*)s.ptr)[0] == '+') k = 1;
      if (k >= s.len) ok = 0;
      for (; ok && k < s.len; k++) {
        uint8_t d = ((uint8_t*)s.ptr)[k];
        if (d < '0' || d > '9') { ok = 0; break; }
        if (v > (~0ULL - (unsigned long long)(d - '0')) / 10ULL) { ok = 0; break; }
        v = v * 10ULL + (unsigned long long)(d - '0');
      }
      if (!ok) { ku_object_drop(headers); free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
      content_length = v;
    }
  }
  if (content_length > (unsigned long long)max_body) { ku_object_drop(headers); free(hdr); ku_http_write_status(cli, 413, "Content Too Large"); return; }
  /* Body: read exactly content_length bytes under read_body_timeout. A timeout or
     peer close before the body completes -> 408 (matches read_exact -> 408). */
  char* body_buf = NULL; size_t body_len = 0;
  if (content_length > 0) {
    body_buf = (char*)malloc((size_t)content_length);
    if (!body_buf) { ku_object_drop(headers); free(hdr); ku_http_write_status(cli, 500, "Internal Server Error"); return; }
    { DWORD t = body_tmo; setsockopt(cli, SOL_SOCKET, SO_RCVTIMEO, (const char*)&t, sizeof(t)); }
    size_t got = 0;
    while (got < (size_t)content_length) {
      /* recv takes an int length, and max_body_bytes is a config value that may
         exceed INT_MAX -- chunk the read instead of overflowing the cast. */
      size_t want = (size_t)content_length - got;
      if (want > (size_t)1048576) want = (size_t)1048576;
      int n = recv(cli, body_buf + got, (int)want, 0);
      if (n <= 0) { free(body_buf); ku_object_drop(headers); free(hdr); ku_http_write_status(cli, 408, "Request Timeout"); return; }
      got += (size_t)n;
    }
    body_len = (size_t)content_length;
  }
  char* qmark = strchr(target, '?');
  size_t path_len = qmark ? (size_t)(qmark - target) : strlen(target);
  /* Sized off the target limit, not a smaller fixed buffer: normalization only
     ever adds a leading '/', so this cannot truncate a target that passed the 414
     check -- and a truncated normalized path would reintroduce exactly the route
     collision the 414 check exists to prevent. */
  char norm[KU_HTTP_MAX_TARGET + 2];
  ku_http_normalize_path(target, path_len, norm, sizeof(norm));
  const char* segs[KU_HTTP_MAX_SEGS]; size_t seglens[KU_HTTP_MAX_SEGS]; size_t nseg = 0;
  { size_t nl = strlen(norm); size_t j = 0;
    while (j < nl && nseg < KU_HTTP_MAX_SEGS) {
      while (j < nl && norm[j] == '/') j++;
      size_t st = j;
      while (j < nl && norm[j] != '/') j++;
      if (j > st) { segs[nseg] = norm + st; seglens[nseg] = j - st; nseg++; }
    }
  }
  KuObject* params = ku_object_new(0);
  KuHttpHandler* route = ku_http_trie_match(server->root, segs, seglens, nseg, 0, method, params);
  if (route) {
    KuStruct___ku_http_response resp;
    if (route->arity == 1) {
      KuStruct___ku_http_request req;
      req.method = ku_http_string_copy(method, strlen(method));
      req.path = ku_http_string_copy(target, path_len);
      req.body = ku_http_string_copy(body_buf ? body_buf : "", body_len);
      req.params = params;
      req.query = ku_http_parse_query(target, path_len);
      req.headers = headers; headers = NULL;
      params = NULL;
      if (route->returns_result) {
        KuResult_struct___ku_http_response rr = ((KuResult_struct___ku_http_response(*)(void*, KuStruct___ku_http_request))route->invoke)(route->env, req);
        resp = ku_http_response_from_result(rr);
      } else {
        resp = ((KuStruct___ku_http_response(*)(void*, KuStruct___ku_http_request))route->invoke)(route->env, req);
      }
    } else {
      ku_object_drop(params); params = NULL;
      if (route->returns_result) {
        KuResult_struct___ku_http_response rr = ((KuResult_struct___ku_http_response(*)(void*))route->invoke)(route->env);
        resp = ku_http_response_from_result(rr);
      } else {
        resp = ((KuStruct___ku_http_response(*)(void*))route->invoke)(route->env);
      }
    }
    ku_http_write_response(cli, &resp);
    ku_drop_struct___ku_http_response(&resp);
  } else {
    ku_object_drop(params); params = NULL;
    if (ku_http_trie_path_exists(server->root, segs, seglens, nseg, 0)) {
      ku_http_write_status(cli, 405, "Method Not Allowed");
    } else {
      ku_http_write_status(cli, 404, "Not Found");
    }
  }
  if (headers) ku_object_drop(headers);
  free(body_buf);
  free(hdr);
}
static KuResult_null ku_http_listen_err(KuHttpServer* server, const char* message) {
  ku_http_server_free(server);
  return (KuResult_null){ false, 0, ku_error_make(ku_string_static((const uint8_t*)"http", 4), ku_string_static((const uint8_t*)"listen_failed", 13), ku_string_static((const uint8_t*)message, strlen(message))) };
}
/* Stage 8d: admission-controlled HTTP server. A single acceptor (the thread that
   called ku_http_listen) owns accept() and runs admission control; a pool of
   handler workers runs the compiled route closures pulled from a bounded queue.
   This split mirrors the interpreter: a dedicated acceptor can always accept a
   connection and answer it (even a 503) while every worker is busy, whereas the
   Stage 8c model (workers accept() directly on a shared listener) could not send
   a 503 once all workers were blocked in a handler.

   Limit mapping, aligned with the interpreter's HttpServerRuntimeLimits:
     * max_connections    -> the process-global atomic "connection permit"
       counter ku_http_active_conns. The acceptor increments it on every accepted
       connection; if the post-increment value exceeds max_connections the
       connection is answered with 503 and closed and the counter rolled back,
       exactly like the interpreter's HttpConnectionPermit CAS.
     * max_pending_requests -> the capacity of the bounded hand-off queue between
       the acceptor and the workers. A connection that took a permit but finds the
       queue full is answered with 503 (the interpreter's connection_tx.try_send
       Full -> 503 path). The OS listen() backlog is set to SOMAXCONN so TCP
       accepts land promptly; the application-level pending bound is this queue.
     * max_active_requests -> the number of handler worker threads = the maximum
       number of route closures running concurrently (capped at
       KU_HTTP_WORKER_CAP so a large config never spawns a runaway thread count).
     * handler_timeout_ms -> read and stored for parity but NOT enforced natively
       for compute-bound handlers. A compiled C route function cannot be preempted
       mid-computation without a data race: a watchdog that closed the socket
       could double-close it against the worker's own close, and a watchdog that
       wrote 504 could interleave with the worker's own response write. Rather
       than risk a double-close / double-write, native documents this as a known
       limitation (the interpreter can enforce it only because it re-checks a
       deadline between interpreter steps). I/O stalls are still bounded by the
       client eventually closing (recv/send then fail and the worker moves on).

   The permit counter is incremented only by the single acceptor thread and
   decremented by whichever thread finishes the connection (acceptor on a reject,
   worker after handling), so InterlockedIncrement/Decrement keep the bound
   race-free without any lock. The routing trie is built before listen and never
   mutated, so all workers read it lock-free. */
#define KU_HTTP_WORKER_CAP 64
/* Process-global connection-permit counter (live accepted-but-not-yet-finished
   connections). Only one server runs per process, so a file-scope global is
   safe; it is touched from the acceptor and every worker, so all updates use the
   atomic Interlocked API on the `volatile LONG` those intrinsics require. */
static volatile LONG ku_http_active_conns = 0;
/* Bounded hand-off queue of client sockets: one producer (the acceptor) pushes,
   N workers pop. A CRITICAL_SECTION guards the ring buffer and a
   CONDITION_VARIABLE parks idle workers; `closed` makes every parked worker wake
   and drain-then-exit at shutdown. */
typedef struct {
  SOCKET* items; int cap; int head; int tail; int count; int closed;
  CRITICAL_SECTION lock; CONDITION_VARIABLE not_empty;
} KuHttpQueue;
static void ku_http_queue_init(KuHttpQueue* q, int cap) {
  if (cap < 1) cap = 1;
  q->items = (SOCKET*)malloc((size_t)cap * sizeof(SOCKET));
  if (!q->items) { fprintf(stderr, "out of memory\n"); exit(1); }
  q->cap = cap; q->head = 0; q->tail = 0; q->count = 0; q->closed = 0;
  InitializeCriticalSection(&q->lock);
  InitializeConditionVariable(&q->not_empty);
}
/* Non-blocking push; returns 0 when the queue is full (pending exhausted). */
static int ku_http_queue_push(KuHttpQueue* q, SOCKET s) {
  int ok = 0;
  EnterCriticalSection(&q->lock);
  if (!q->closed && q->count < q->cap) {
    q->items[q->tail] = s; q->tail = (q->tail + 1) % q->cap; q->count++;
    WakeConditionVariable(&q->not_empty); ok = 1;
  }
  LeaveCriticalSection(&q->lock);
  return ok;
}
/* Blocking pop; returns INVALID_SOCKET only when the queue is closed AND drained,
   which is the worker's signal to exit. */
static SOCKET ku_http_queue_pop(KuHttpQueue* q) {
  SOCKET s = INVALID_SOCKET;
  EnterCriticalSection(&q->lock);
  while (q->count == 0 && !q->closed)
    SleepConditionVariableCS(&q->not_empty, &q->lock, INFINITE);
  if (q->count > 0) {
    s = q->items[q->head]; q->head = (q->head + 1) % q->cap; q->count--;
  }
  LeaveCriticalSection(&q->lock);
  return s;
}
static void ku_http_queue_close(KuHttpQueue* q) {
  EnterCriticalSection(&q->lock);
  q->closed = 1;
  WakeAllConditionVariable(&q->not_empty);
  LeaveCriticalSection(&q->lock);
}
/* Free the queue after workers have joined. Any still-buffered sockets (workers
   normally drain them all before exit) are closed and their permits rolled back
   so shutdown leaks nothing. */
static void ku_http_queue_free(KuHttpQueue* q) {
  while (q->count > 0) {
    SOCKET s = q->items[q->head]; q->head = (q->head + 1) % q->cap; q->count--;
    closesocket(s);
    InterlockedDecrement(&ku_http_active_conns);
  }
  DeleteCriticalSection(&q->lock);
  free(q->items); q->items = NULL;
}
typedef struct { KuHttpServer* server; KuHttpQueue* queue; } KuHttpWorkerCtx;
static unsigned __stdcall ku_http_worker(void* arg) {
  KuHttpWorkerCtx* ctx = (KuHttpWorkerCtx*)arg;
  for (;;) {
    SOCKET cli = ku_http_queue_pop(ctx->queue);
    if (cli == INVALID_SOCKET) break;              /* queue closed and drained */
    ku_http_handle_connection(ctx->server, cli);
    closesocket(cli);
    InterlockedDecrement(&ku_http_active_conns);   /* release the connection permit */
  }
  return 0;
}
/* Answer an over-limit connection with 503 and release its permit. Shared by the
   max_connections and max_pending rejection paths so the close + decrement stay
   in one place. */
static void ku_http_reject_503(KuHttpServer* server, SOCKET cli) {
  /* Mirrors the interpreter's reject_http_connection: drain the in-flight request
     under a 10ms read timeout, bounded by max_header_bytes and stopping at the
     header terminator, then write 503 and half-close.

     Draining BEFORE the write matters: closesocket() with unread request bytes
     still in the receive buffer makes Windows abort with an RST, which discards
     the 503 (the client sees a connection reset instead of the status). Bounding
     the drain matters too: an unbounded `while (recv(..) > 0)` lets a peer that
     keeps streaming pin this thread indefinitely. */
  long long max_header = server->max_header_bytes > 0 ? server->max_header_bytes : (16 * 1024);
  { DWORD tmo = 10; setsockopt(cli, SOL_SOCKET, SO_RCVTIMEO, (const char*)&tmo, sizeof(tmo)); }
  { char drain[1024]; long long received = 0; unsigned char w[4] = {0, 0, 0, 0}; long long seen = 0; int done = 0;
    while (received < max_header && !done) {
      int n = recv(cli, drain, (int)sizeof(drain), 0);
      if (n <= 0) break;
      received += (long long)n;
      for (int i = 0; i < n && !done; i++) {
        w[0] = w[1]; w[1] = w[2]; w[2] = w[3]; w[3] = (unsigned char)drain[i]; seen++;
        if (seen >= 4 && w[0] == '\r' && w[1] == '\n' && w[2] == '\r' && w[3] == '\n') done = 1;
        else if (seen >= 2 && w[2] == '\n' && w[3] == '\n') done = 1;
      }
    }
  }
  { DWORD tmo = (DWORD)(server->write_timeout_ms > 0 ? server->write_timeout_ms : 10000);
    setsockopt(cli, SOL_SOCKET, SO_SNDTIMEO, (const char*)&tmo, sizeof(tmo)); }
  ku_http_write_status(cli, 503, "Service Unavailable");
  shutdown(cli, SD_SEND);
  closesocket(cli);
  InterlockedDecrement(&ku_http_active_conns);
}
static KuResult_null ku_http_listen(KuHttpServer* server, KuString address) {
  char* addr = ku_string_to_cstr(address);
  ku_string_drop(&address);
  unsigned long host = htonl(INADDR_LOOPBACK);
  int port = 8080;
  char* colon = strrchr(addr, ':');
  if (colon) {
    *colon = '\0'; port = atoi(colon + 1);
    if (strcmp(addr, "0.0.0.0") == 0) host = htonl(INADDR_ANY);
    else if (addr[0]) { unsigned long a = inet_addr(addr); if (a != INADDR_NONE) host = a; }
  } else { port = atoi(addr); }
  free(addr);
  WSADATA wsa;
  if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return ku_http_listen_err(server, "WSAStartup failed");
  SOCKET srv = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (srv == INVALID_SOCKET) { WSACleanup(); return ku_http_listen_err(server, "socket failed"); }
  BOOL yes = 1;
  setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, (const char*)&yes, sizeof(yes));
  struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET; sa.sin_addr.s_addr = host; sa.sin_port = htons((unsigned short)port);
  if (bind(srv, (struct sockaddr*)&sa, sizeof(sa)) == SOCKET_ERROR) { closesocket(srv); WSACleanup(); return ku_http_listen_err(server, "bind failed"); }
  if (listen(srv, SOMAXCONN) == SOCKET_ERROR) { closesocket(srv); WSACleanup(); return ku_http_listen_err(server, "listen failed"); }
  long max_requests = 0;
  { const char* mr = getenv("KU_HTTP_MAX_REQUESTS"); if (mr) max_requests = atol(mr); }
  /* Resolve admission-control limits from the server config (defaults already
     applied in ku_http_server_new). */
  /* The limits are i64 (as in the interpreter), but the permit counter is a 32-bit
     LONG and the queue/worker counts are ints -- clamp instead of casting, so a
     limit above INT_MAX cannot truncate to a negative value and silently turn
     into "reject everything" / "queue of 1". */
  long long mc = server->max_connections > 0 ? server->max_connections : 1024;
  if (mc > 0x7fffffffLL) mc = 0x7fffffffLL;
  long max_conn = (long)mc;
  long long mp = server->max_pending_requests > 0 ? server->max_pending_requests : 1;
  if (mp > 0x7fffffffLL) mp = 0x7fffffffLL;
  int max_pending = (int)mp;
  long long mw = server->max_active_requests > 0 ? server->max_active_requests : 1;
  if (mw > 0x7fffffffLL) mw = 0x7fffffffLL;
  int nworkers = (int)mw;
  if (nworkers > KU_HTTP_WORKER_CAP) nworkers = KU_HTTP_WORKER_CAP;
  if (nworkers < 1) nworkers = 1;
  ku_http_active_conns = 0;
  KuHttpQueue queue; ku_http_queue_init(&queue, max_pending);
  KuHttpWorkerCtx ctx; ctx.server = server; ctx.queue = &queue;
  HANDLE* workers = (HANDLE*)malloc((size_t)nworkers * sizeof(HANDLE));
  int spawned = 0;
  if (workers) {
    for (int w = 0; w < nworkers; w++) {
      uintptr_t h = _beginthreadex(NULL, 0, ku_http_worker, &ctx, 0, NULL);
      if (h == 0) break;
      workers[spawned++] = (HANDLE)h;
    }
  }
  /* Acceptor loop. Blocking accept() on this thread means listen() blocks here
     until the process is killed (resident server) or KU_HTTP_MAX_REQUESTS
     connections have been accepted (finite smoke/ASan runs). */
  long accepted = 0;
  for (;;) {
    SOCKET cli = accept(srv, NULL, NULL);
    if (cli == INVALID_SOCKET) break;
    if (max_requests > 0) accepted++;
    LONG live = InterlockedIncrement(&ku_http_active_conns);   /* connection permit */
    if (live > max_conn) {
      ku_http_reject_503(server, cli);            /* max_connections exceeded */
    } else if (spawned == 0) {
      /* Degenerate fallback: no worker threads could be spawned, so serve inline
         on the acceptor. Still correct (serial), permit released after handling. */
      ku_http_handle_connection(server, cli);
      closesocket(cli);
      InterlockedDecrement(&ku_http_active_conns);
    } else if (!ku_http_queue_push(&queue, cli)) {
      ku_http_reject_503(server, cli);            /* max_pending exceeded */
    }
    if (max_requests > 0 && accepted >= max_requests) break;
  }
  closesocket(srv);
  /* Signal workers to drain the queue then exit, and join them before freeing any
     shared state so no worker touches a freed server/queue. */
  ku_http_queue_close(&queue);
  if (spawned > 0) {
    WaitForMultipleObjects((DWORD)spawned, workers, TRUE, INFINITE);
    for (int w = 0; w < spawned; w++) CloseHandle(workers[w]);
  }
  free(workers);
  ku_http_queue_free(&queue);
  ku_http_server_free(server);
  WSACleanup();
  return (KuResult_null){ true, 0, (KuError){0} };
}

"####,
    );
    Ok(())
}

/// Emit `ku_array_try_get_*` after both the array and result ABIs. Restricted to
/// element types that have BOTH a `KuArray_E` and a `KuResult_E` — guaranteed
/// whenever the program calls `array.try_get`, which produces `Result<element>`.
/// `nums[i]` stays a hard bounds abort; `nums.try_get(i)` is the recoverable read
/// returning `Err{domain:"array", code:"index_out_of_bounds"}`.
fn emit_array_try_get_helpers(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut array_elements = Vec::new();
    collect_array_elements_program(program, &mut array_elements);
    let mut result_inners = Vec::new();
    collect_result_inners_program(program, &mut result_inners)?;

    let mut emitted = false;
    for element in &array_elements {
        if !result_inners.contains(element) {
            continue;
        }
        let suffix = c_type_suffix(element)?;
        let array_type = c_array_type(element)?;
        let result_type = c_result_type(element)?;
        let zero = c_zero_value(element)?;
        let clone = c_clone_value(element, "array.data[index]")?;
        out.push_str(&format!(
            "static {result_type} ku_array_try_get_{suffix}({array_type} array, int64_t index) {{\n\
             \x20 if (index < 0 || (uint64_t)index >= array.len) {{\n\
             \x20   return ({result_type}){{ false, {zero}, ku_error_make(ku_string_static((const uint8_t*)\"array\", 5), ku_string_static((const uint8_t*)\"index_out_of_bounds\", 19), ku_string_static((const uint8_t*)\"array index out of bounds\", 25)) }};\n\
             \x20 }}\n\
             \x20 return ({result_type}){{ true, {clone}, (KuError){{0}} }};\n\
             }}\n"
        ));
        emitted = true;
    }
    if emitted {
        out.push('\n');
    }
    Ok(())
}

/// Emit `ku_string_slice` after the Result ABI (it returns `KuResult_str`, so it is
/// only emitted when the program produces a `Result<str>` — guaranteed whenever it
/// calls `.slice`). Char-indexed like the interpreter: bounds errors carry the same
/// domain/code/message so a caught error reads identically to `ku run`.
fn emit_string_slice_helper(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut result_inners = Vec::new();
    collect_result_inners_program(program, &mut result_inners)?;
    if !result_inners.contains(&IrType::Str) {
        return Ok(());
    }
    out.push_str(
        "static KuResult_str ku_string_slice(KuString s, int64_t start, int64_t end) {\n\
        \x20 if (start < 0 || end < 0) {\n\
        \x20   return (KuResult_str){ false, (KuString){0}, ku_error_make(ku_string_static((const uint8_t*)\"string\", 6), ku_string_static((const uint8_t*)\"slice_error\", 11), ku_string_static((const uint8_t*)\"string.slice indexes must be >= 0\", 33)) };\n\
        \x20 }\n\
        \x20 if (start > end) {\n\
        \x20   return (KuResult_str){ false, (KuString){0}, ku_error_make(ku_string_static((const uint8_t*)\"string\", 6), ku_string_static((const uint8_t*)\"slice_error\", 11), ku_string_static((const uint8_t*)\"string.slice start must be <= end\", 33)) };\n\
        \x20 }\n\
        \x20 size_t clen = ku_string_char_len(s);\n\
        \x20 if ((uint64_t)end > (uint64_t)clen) {\n\
        \x20   char buf[96]; int n = snprintf(buf, sizeof(buf), \"string.slice end %lld out of bounds for length %zu\", (long long)end, clen);\n\
        \x20   KuString msg = (KuString){0}; if (n > 0) { msg = ku_string_alloc((size_t)n); memcpy(msg.ptr, buf, (size_t)n); }\n\
        \x20   return (KuResult_str){ false, (KuString){0}, ku_error_make(ku_string_static((const uint8_t*)\"string\", 6), ku_string_static((const uint8_t*)\"slice_error\", 11), msg) };\n\
        \x20 }\n\
        \x20 size_t bi = 0, ci = 0;\n\
        \x20 while (bi < s.len && ci < (size_t)start) { bi++; while (bi < s.len && (s.ptr[bi] & 0xC0) == 0x80) bi++; ci++; }\n\
        \x20 size_t bstart = bi;\n\
        \x20 while (bi < s.len && ci < (size_t)end) { bi++; while (bi < s.len && (s.ptr[bi] & 0xC0) == 0x80) bi++; ci++; }\n\
        \x20 size_t blen = bi - bstart;\n\
        \x20 KuString outv = ku_string_alloc(blen);\n\
        \x20 if (blen) memcpy(outv.ptr, s.ptr + bstart, blen);\n\
        \x20 return (KuResult_str){ true, outv, (KuError){0} };\n\
        }\n\n",
    );
    Ok(())
}

fn collect_array_elements_program(program: &IrProgram, output: &mut Vec<IrType>) {
    for function in &program.functions {
        collect_array_element_type(&function.return_type, output);
        for param in &function.params {
            collect_array_element_type(&param.ty, output);
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
                        collect_array_element_type(ty, output);
                        collect_array_expr_types(value, output);
                    }
                    IrInst::BindOk { ty, result, .. } => {
                        collect_array_element_type(ty, output);
                        collect_array_expr_types(result, output);
                    }
                    IrInst::Store { target, value } => {
                        collect_array_lvalue_types(target, output);
                        collect_array_expr_types(value, output);
                    }
                    IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value) => collect_array_expr_types(value, output),
                    IrInst::CellNew { init, .. } => collect_array_expr_types(init, output),
                    IrInst::CellStore { cell, value } => {
                        collect_array_expr_types(cell, output);
                        collect_array_expr_types(value, output);
                    }
                    IrInst::BeginTry { .. }
                    | IrInst::EndTry
                    | IrInst::BindError { .. }
                    | IrInst::DefineClosure { .. }
                    | IrInst::CellRelease(_)
                    | IrInst::Unsupported { .. } => {}
                }
            }
        }
    }
}

fn collect_result_inners_program(
    program: &IrProgram,
    output: &mut Vec<IrType>,
) -> KuResult<()> {
    for function in &program.functions {
        collect_result_type(&function.return_type, output)?;
        for param in &function.params {
            collect_result_type(&param.ty, output)?;
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
                        collect_result_type(ty, output)?;
                        collect_result_type(&value.ty, output)?;
                    }
                    IrInst::BindOk { result, .. } => collect_result_type(&result.ty, output)?,
                    IrInst::Store { value, .. }
                    | IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value) => collect_result_type(&value.ty, output)?,
                    IrInst::CellNew { init, .. } => collect_result_type(&init.ty, output)?,
                    IrInst::CellStore { cell, value } => {
                        collect_result_type(&cell.ty, output)?;
                        collect_result_type(&value.ty, output)?;
                    }
                    IrInst::BeginTry { .. }
                    | IrInst::EndTry
                    | IrInst::BindError { .. }
                    | IrInst::DefineClosure { .. }
                    | IrInst::CellRelease(_)
                    | IrInst::Unsupported { .. } => {}
                }
            }
            match &block.terminator {
                IrTerminator::ResultBranch { result, .. }
                | IrTerminator::JumpErr { result, .. }
                | IrTerminator::PropagateErr(result)
                | IrTerminator::Return(Some(result)) => collect_result_type(&result.ty, output)?,
                IrTerminator::Branch { condition, .. } => {
                    collect_result_type(&condition.ty, output)?
                }
                IrTerminator::ForEach { iterable, .. } => {
                    collect_result_type(&iterable.ty, output)?
                }
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Return(None)
                | IrTerminator::Unreachable => {}
            }
        }
    }
    Ok(())
}

fn collect_result_type(ty: &IrType, result_types: &mut Vec<IrType>) -> KuResult<()> {
    match ty {
        IrType::Result(inner) => {
            match inner.as_ref() {
                IrType::Int
                | IrType::Bool
                | IrType::Str
                | IrType::Null
                | IrType::Array(_)
                | IrType::Named(_) => {}
                _ => {
                    return Err(unsupported(format!(
                        "native C prototype does not support Result<{inner}>"
                    )))
                }
            }
            if !result_types.contains(inner.as_ref()) {
                result_types.push(*inner.clone());
            }
        }
        IrType::Array(inner) => collect_result_type(inner, result_types)?,
        _ => {}
    }
    Ok(())
}

fn c_result_type(inner: &IrType) -> KuResult<String> {
    Ok(format!("KuResult_{}", c_type_suffix(inner)?))
}

fn emit_main_wrapper(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let Some(function) = program
        .functions
        .iter()
        .find(|function| function.name == "main")
    else {
        return Ok(());
    };
    if !function.params.is_empty() {
        return Err(unsupported(
            "native C main wrapper does not support main parameters",
        ));
    }
    out.push_str("int main(void) {\n");
    match &function.return_type {
        IrType::Void => {
            out.push_str("  ku_main();\n  return 0;\n");
        }
        IrType::Int => {
            out.push_str("  return (int)ku_main();\n");
        }
        IrType::Bool => {
            out.push_str("  return ku_main() ? 0 : 1;\n");
        }
        IrType::Str => {
            out.push_str("  KuString result = ku_main();\n  printf(\"%.*s\\n\", (int)result.len, (const char*)result.ptr);\n  ku_string_drop(&result);\n  return 0;\n");
        }
        IrType::Result(inner) => {
            out.push_str(&format!(
                "  {} result = ku_main();\n  if (!result.ok) {{ fprintf(stderr, \"%.*s\\n\", (int)result.error.message.len, (const char*)result.error.message.ptr); ku_result_drop_{}(&result); return 1; }}\n  ku_result_drop_{}(&result);\n  return 0;\n",
                c_type(&function.return_type)?,
                c_type_suffix(inner)?,
                c_type_suffix(inner)?
            ));
        }
        other => {
            return Err(unsupported(format!(
                "native C main wrapper does not support main return type {other}"
            )));
        }
    }
    out.push_str("}\n");
    Ok(())
}

/// A static-storage `KuString` C literal for a fixed ASCII string.
fn c_static_string(text: &str) -> String {
    format!(
        "ku_string_static((const uint8_t*)\"{}\", {})",
        text.replace('\\', "\\\\").replace('"', "\\\""),
        text.len()
    )
}

/// Emit a `ku_string_static(...)` for an IR string-literal text. The IR stores the
/// literal Rust-`Debug`-quoted (e.g. `"x\u{a0}y"`), but `\u{NN}` is NOT valid C —
/// MSVC would drop the backslash and corrupt the string (a silent native≠interpreter
/// divergence on NBSP / combining marks / zero-width / control chars). Decode to the
/// real bytes and re-emit them octal-escaped, with an explicit byte length (so
/// embedded NULs and multibyte sequences survive `sizeof`-free).
fn c_str_literal_static(value: &str) -> KuResult<String> {
    let bytes = crate::backend::llvm::decode_string_literal(value)?;
    let mut lit = String::from("\"");
    for &b in &bytes {
        match b {
            b'"' => lit.push_str("\\\""),
            b'\\' => lit.push_str("\\\\"),
            b'\n' => lit.push_str("\\n"),
            b'\r' => lit.push_str("\\r"),
            b'\t' => lit.push_str("\\t"),
            0x20..=0x7e => lit.push(b as char),
            // 3-digit octal is unambiguous in C (it consumes at most 3 digits), so it
            // is safe even when the next source char is another digit.
            _ => lit.push_str(&format!("\\{b:03o}")),
        }
    }
    lit.push('"');
    Ok(format!(
        "ku_string_static((const uint8_t*){lit}, {})",
        bytes.len()
    ))
}

/// Stage 8a: lower the native HTTP builtins. Returns `Some(expr)` when `name` is
/// an HTTP intrinsic, `None` otherwise so the generic dispatch continues.
fn c_http_intrinsic_expr(name: &str, args: &[IrExpr]) -> KuResult<Option<String>> {
    let resp_ty = "KuStruct___ku_http_response";
    // http.server() / http.service(): a fresh empty route table. Stage 8d: when a
    // config object is passed, read its admission-control limits (max_connections,
    // max_active_requests, max_pending_requests, handler_timeout_ms) into the
    // server. The config object is READ, not moved — it stays owned by the caller,
    // which drops it after this call returns.
    if name == "http.server" || name == "http.service" {
        if let Some(config) = args.first() {
            return Ok(Some(format!("ku_http_server_new_cfg({})", c_expr(config)?)));
        }
        return Ok(Some("ku_http_server_new()".to_string()));
    }
    // `req.params.<k>` / `req.query.<k>` / `req.headers.<k>`: borrow the request map
    // and copy out the string value (empty when absent). The object is read, not
    // moved (it stays owned by the request struct); the key is a static literal.
    if name == "__ku_http_map_get" {
        let object = args
            .first()
            .ok_or_else(|| unsupported("native C __ku_http_map_get requires a map"))?;
        let key = args
            .get(1)
            .ok_or_else(|| unsupported("native C __ku_http_map_get requires a key"))?;
        return Ok(Some(format!(
            "ku_http_map_get({}, {})",
            c_expr(object)?,
            c_expr(key)?
        )));
    }
    // Response helpers build a `KuHttpResponse` value directly (no dynamic object).
    // Each supports the 1-arg body form and the 2-arg `(status, body)` form the
    // interpreter accepts.
    if matches!(name, "http.text" | "http.html") {
        let content_type = if name == "http.text" {
            "text/plain; charset=utf-8"
        } else {
            "text/html; charset=utf-8"
        };
        let (status, body) = match args {
            [body] => ("200".to_string(), c_value_expr(body)?),
            [status, body] => (c_expr(status)?, c_value_expr(body)?),
            _ => return Err(unsupported(format!("native C {name} expects 1 or 2 arguments"))),
        };
        return Ok(Some(format!(
            "({resp_ty}){{ {status}, {}, {body}, (KuString){{0}} }}",
            c_static_string(content_type)
        )));
    }
    if name == "http.empty" {
        let status = match args {
            [] => "204".to_string(),
            [status] => c_expr(status)?,
            _ => return Err(unsupported("native C http.empty expects 0 or 1 arguments")),
        };
        return Ok(Some(format!(
            "({resp_ty}){{ {status}, (KuString){{0}}, (KuString){{0}}, (KuString){{0}} }}"
        )));
    }
    if name == "http.redirect" {
        let (status, location) = match args {
            [location] => ("302".to_string(), c_value_expr(location)?),
            [status, location] => (c_expr(status)?, c_value_expr(location)?),
            _ => return Err(unsupported("native C http.redirect expects 1 or 2 arguments")),
        };
        return Ok(Some(format!(
            "({resp_ty}){{ {status}, (KuString){{0}}, (KuString){{0}}, {location} }}"
        )));
    }
    // app.listen(address): run the single-threaded accept loop.
    if name == "__ku_http_listen" {
        let server = args
            .first()
            .ok_or_else(|| unsupported("native C http listen requires a server"))?;
        let address = args
            .get(1)
            .ok_or_else(|| unsupported("native C http listen requires an address"))?;
        return Ok(Some(format!(
            "ku_http_listen({}, {})",
            c_expr(server)?,
            c_value_expr(address)?
        )));
    }
    // app.get/post/...(path, handler): register a route. The intrinsic name carries
    // the HTTP method, the handler arity (0/1) and whether it returns a Result.
    if let Some(rest) = name.strip_prefix("__ku_http_route:") {
        let mut parts = rest.splitn(3, ':');
        let method = parts
            .next()
            .ok_or_else(|| unsupported("invalid native http route intrinsic"))?;
        let arity = parts
            .next()
            .ok_or_else(|| unsupported("invalid native http route intrinsic"))?;
        let returns_result = parts
            .next()
            .ok_or_else(|| unsupported("invalid native http route intrinsic"))?;
        let server = args
            .first()
            .ok_or_else(|| unsupported("native C http route requires a server"))?;
        let path = args
            .get(1)
            .ok_or_else(|| unsupported("native C http route requires a path"))?;
        let handler = args
            .get(2)
            .ok_or_else(|| unsupported("native C http route requires a handler"))?;
        // Read (not move) the closure value: env ownership is transferred to the
        // route table, which releases it when the server is freed.
        let handler = c_expr(handler)?;
        return Ok(Some(format!(
            "ku_http_server_add_route({}, {}, {}, (void*)({handler}).invoke, ({handler}).env, {arity}, {returns_result})",
            c_expr(server)?,
            c_static_string(method),
            c_value_expr(path)?
        )));
    }
    Ok(None)
}

/// Lower a `pg.<method>` intrinsic (the thin libpq binding). The connection/result
/// are borrowed (`c_expr`) for reads; `pg.close` consumes the connection with
/// `c_value_expr` (move-and-clear) so the scope-end drop cannot double-`PQfinish`.
fn c_pg_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("pg.{method} missing argument")))
    };
    match method {
        "connect" => Ok(format!("ku_pg_connect({})", c_expr(arg(0)?)?)),
        "query" => Ok(format!(
            "ku_pg_query({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "query_params" => Ok(format!(
            "ku_pg_query_params({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "rows" => Ok(format!("ku_pg_rows({})", c_expr(arg(0)?)?)),
        "cols" => Ok(format!("ku_pg_cols({})", c_expr(arg(0)?)?)),
        "value" => Ok(format!(
            "ku_pg_value({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "close" => Ok(format!("ku_pg_close({})", c_value_expr(arg(0)?)?)),
        "pool" => Ok(format!("ku_pg_pool({}, {})", c_expr(arg(0)?)?, c_expr(arg(1)?)?)),
        "pool_query" => Ok(format!(
            "ku_pg_pool_query({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "pool_query_params" => Ok(format!(
            "ku_pg_pool_query_params({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "pool_close" => Ok(format!("ku_pg_pool_close({})", c_value_expr(arg(0)?)?)),
        other => Err(unsupported(format!("native C pg.{other}() is not implemented"))),
    }
}

/// Lower a `mysql.<method>` intrinsic (thin libmysqlclient binding). Handles are
/// borrowed for reads; `mysql.close` consumes the connection (move-and-clear).
fn c_mysql_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("mysql.{method} missing argument")))
    };
    match method {
        "connect" => Ok(format!(
            "ku_mysql_connect({}, {}, {}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?,
            c_expr(arg(3)?)?,
            c_expr(arg(4)?)?
        )),
        "query" => Ok(format!("ku_mysql_query({}, {})", c_expr(arg(0)?)?, c_expr(arg(1)?)?)),
        "query_params" => Ok(format!(
            "ku_mysql_query_params({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "rows" => Ok(format!("ku_mysql_rows({})", c_expr(arg(0)?)?)),
        "cols" => Ok(format!("ku_mysql_cols({})", c_expr(arg(0)?)?)),
        "value" => Ok(format!(
            "ku_mysql_value({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "close" => Ok(format!("ku_mysql_close({})", c_value_expr(arg(0)?)?)),
        other => Err(unsupported(format!(
            "native C mysql.{other}() is not implemented"
        ))),
    }
}

/// Lower a `redis.<method>` intrinsic (RESP over Winsock). The connection is borrowed
/// for commands; `redis.close` consumes it (move-and-clear) so scope-end drop cannot
/// double-close the socket.
fn c_redis_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("redis.{method} missing argument")))
    };
    match method {
        "connect" => Ok(format!(
            "ku_redis_connect({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "auth" => Ok(format!("ku_redis_auth({}, {})", c_expr(arg(0)?)?, c_expr(arg(1)?)?)),
        "get" => Ok(format!("ku_redis_get({}, {})", c_expr(arg(0)?)?, c_expr(arg(1)?)?)),
        "set" => Ok(format!(
            "ku_redis_set({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "del" => Ok(format!("ku_redis_del({}, {})", c_expr(arg(0)?)?, c_expr(arg(1)?)?)),
        "close" => Ok(format!("ku_redis_close({})", c_value_expr(arg(0)?)?)),
        other => Err(unsupported(format!(
            "native C redis.{other}() is not implemented"
        ))),
    }
}

/// Lower a `string.<method>` intrinsic. The receiver and any string arguments are
/// read as borrows (`c_expr`): the helpers only read them, so the caller's cleanup
/// still owns and drops the originals. Byte/codepoint-exact to the interpreter's
/// Rust semantics; the Unicode-table-dependent methods (trim/lower/upper) are loud
/// gaps rather than silent ASCII-only divergences.
fn c_string_method_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let recv = args
        .first()
        .ok_or_else(|| unsupported("string method requires a receiver"))?;
    let arg = |i: usize| -> KuResult<String> {
        c_expr(
            args.get(i)
                .ok_or_else(|| unsupported("string method missing argument"))?,
        )
    };
    match method {
        // `len` counts Unicode scalar values, matching Rust's `chars().count()`.
        "len" => Ok(format!("(int64_t)ku_string_char_len({})", c_expr(recv)?)),
        "contains" => Ok(format!(
            "ku_string_contains({}, {})",
            c_expr(recv)?,
            arg(1)?
        )),
        "starts_with" => Ok(format!(
            "ku_string_starts_with({}, {})",
            c_expr(recv)?,
            arg(1)?
        )),
        "ends_with" => Ok(format!(
            "ku_string_ends_with({}, {})",
            c_expr(recv)?,
            arg(1)?
        )),
        "replace" => Ok(format!(
            "ku_string_replace({}, {}, {})",
            c_expr(recv)?,
            arg(1)?,
            arg(2)?
        )),
        "slice" => Ok(format!(
            "ku_string_slice({}, {}, {})",
            c_expr(recv)?,
            arg(1)?,
            arg(2)?
        )),
        // Exact parity needs a Unicode case/whitespace table; ASCII-only versions
        // would silently diverge from the interpreter on non-ASCII input.
        "trim" | "lower" | "upper" => Err(unsupported(format!(
            "native C string.{method}() needs Unicode support and is not implemented yet"
        ))),
        other => Err(unsupported(format!(
            "native C string.{other}() is not implemented yet"
        ))),
    }
}

fn c_intrinsic_expr(name: &str, args: &[IrExpr], ty: &IrType) -> KuResult<String> {
    if let Some(expr) = c_http_intrinsic_expr(name, args)? {
        return Ok(expr);
    }
    if name == "__ku_error_make" {
        if args.len() != 3 {
            return Err(unsupported(
                "native C __ku_error_make requires domain, code, and message",
            ));
        }
        // Move each string into the error; the KuError owns its KuStrings.
        return Ok(format!(
            "ku_error_make({}, {}, {})",
            c_value_expr(&args[0])?,
            c_value_expr(&args[1])?,
            c_value_expr(&args[2])?
        ));
    }
    if name == "__ku_object_get" {
        let object = args
            .first()
            .ok_or_else(|| unsupported("native C __ku_object_get requires an object"))?;
        let key = args
            .get(1)
            .ok_or_else(|| unsupported("native C __ku_object_get requires a key"))?;
        // The object is borrowed (read-only get); the key is a borrowed view.
        return Ok(format!(
            "ku_object_get_result({}, {})",
            c_expr(object)?,
            c_expr(key)?
        ));
    }
    if let Some(method) = name.strip_prefix("object.") {
        let object = args
            .first()
            .ok_or_else(|| unsupported("native C object method requires a receiver"))?;
        match method {
            "get_or" => {
                let key = args
                    .get(1)
                    .ok_or_else(|| unsupported("native C object.get_or requires a key"))?;
                let default = args
                    .get(2)
                    .ok_or_else(|| unsupported("native C object.get_or requires a default"))?;
                return Ok(format!(
                    "ku_object_get_or({}, {}, {})",
                    c_expr(object)?,
                    c_expr(key)?,
                    ku_value_wrap(&default.ty, &c_value_expr(default)?)?
                ));
            }
            other => {
                return Err(unsupported(format!(
                    "native C prototype does not support object method '{other}' yet"
                )))
            }
        }
    }
    if let Some(method) = name.strip_prefix("kuvalue.") {
        let value = args
            .first()
            .ok_or_else(|| unsupported("native C kuvalue method requires a receiver"))?;
        match method {
            "as_int" => return Ok(format!("ku_value_as_int({})", c_value_expr(value)?)),
            "as_str" => return Ok(format!("ku_value_as_str({})", c_value_expr(value)?)),
            other => {
                return Err(unsupported(format!(
                    "native C prototype does not support kuvalue method '{other}' yet"
                )))
            }
        }
    }
    if let Some(rest) = name.strip_prefix("__ku_enum:") {
        let mut parts = rest.splitn(4, ':');
        let enum_name = parts
            .next()
            .ok_or_else(|| unsupported("invalid native enum constructor"))?;
        let variant = parts
            .next()
            .ok_or_else(|| unsupported("invalid native enum constructor"))?;
        let tag = parts
            .next()
            .ok_or_else(|| unsupported("invalid native enum constructor"))?;
        let fields = parts.next().unwrap_or_default();
        let field_names = if fields.is_empty() {
            Vec::new()
        } else {
            fields.split(',').collect::<Vec<_>>()
        };
        if field_names.len() != args.len() {
            return Err(unsupported(format!(
                "native enum constructor '{enum_name}.{variant}' payload metadata mismatch"
            )));
        }
        let mut initializer = format!("({}){{ .tag = {tag}", c_type(ty)?);
        if !args.is_empty() {
            // The enum payload TAKES OWNERSHIP of each argument, so move it in
            // (clearing the source) rather than shallow-copying: `c_expr` would
            // leave the source binding/temp still owning the same heap buffer, and
            // when both it and the value extracted from the enum are later dropped
            // the buffer is freed twice.
            let fields = field_names
                .iter()
                .zip(args)
                .map(|(field, value)| Ok(format!(".{} = {}", c_ident(field), c_value_expr(value)?)))
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            initializer.push_str(&format!(", .payload.{} = {{ {fields} }}", c_ident(variant)));
        }
        initializer.push_str(" }");
        return Ok(initializer);
    }
    if name == "__ku_enum_tag" {
        let value = args
            .first()
            .ok_or_else(|| unsupported("enum tag requires one argument"))?;
        return Ok(format!("({}).tag", c_expr(value)?));
    }
    if let Some(rest) = name.strip_prefix("__ku_enum_payload:") {
        let (variant, field) = rest
            .split_once(':')
            .ok_or_else(|| unsupported("invalid native enum payload access"))?;
        let value = args
            .first()
            .ok_or_else(|| unsupported("enum payload access requires one argument"))?;
        // Binding a payload in a `match` arm MOVES it out of the enum (Ku matches
        // by value), so clear the enum's slot: the enum is deep-dropped afterwards
        // and would otherwise free the same buffer the binding now owns.
        let place = format!(
            "({}).payload.{}.{}",
            c_expr(value)?,
            c_ident(variant),
            c_ident(field)
        );
        return c_move_value(ty, &place);
    }
    if let Some(method) = name.strip_prefix("array.") {
        let receiver = args
            .first()
            .ok_or_else(|| unsupported(format!("native C array.{method} requires a receiver")))?;
        let IrType::Array(element) = &receiver.ty else {
            return Err(unsupported(format!(
                "native C array.{method} requires an array receiver"
            )));
        };
        let suffix = c_type_suffix(element)?;
        // The receiver is borrowed (c_expr, never moved): len/is_empty only read
        // it, and push clones its elements internally, returning a new array.
        match method {
            "len" => return Ok(format!("ku_array_len_{}({})", suffix, c_expr(receiver)?)),
            "is_empty" => {
                return Ok(format!("ku_array_is_empty_{}({})", suffix, c_expr(receiver)?))
            }
            "push" => {
                let value = args.get(1).ok_or_else(|| {
                    unsupported("native C array.push requires a value argument")
                })?;
                return Ok(format!(
                    "ku_array_push_{}({}, {})",
                    suffix,
                    c_expr(receiver)?,
                    c_expr(value)?
                ));
            }
            "try_get" => {
                let index = args.get(1).ok_or_else(|| {
                    unsupported("native C array.try_get requires an index argument")
                })?;
                return Ok(format!(
                    "ku_array_try_get_{}({}, {})",
                    suffix,
                    c_expr(receiver)?,
                    c_expr(index)?
                ));
            }
            // Stage 6f: `arr.map(closure)` calls a per-signature helper that loops
            // over the (borrowed) input and fills a fresh result array by invoking
            // the mapper once per element. The input array is borrowed (`c_expr`,
            // header copy, never freed by the helper); the mapper is passed by the
            // usual call-argument ownership (`c_arg_value_expr`: a temporary literal
            // is moved, a named binding is retained) and the helper releases its one
            // reference exactly once at the end.
            "map" => {
                let mapper = args.get(1).ok_or_else(|| {
                    unsupported("native C array.map requires a mapper closure")
                })?;
                let IrType::Closure { params, ret } = &mapper.ty else {
                    return Err(unsupported(
                        "native C array.map requires a closure argument",
                    ));
                };
                let cl_suffix = closure_signature_suffix(params, ret)?;
                return Ok(format!(
                    "ku_array_map_{}({}, {})",
                    cl_suffix,
                    c_expr(receiver)?,
                    c_arg_value_expr(mapper)?
                ));
            }
            other => {
                return Err(unsupported(format!(
                    "native C prototype does not support array method '{other}' yet"
                )))
            }
        }
    }
    if name == "time.millis" && args.is_empty() {
        return Ok("ku_time_now_millis()".to_string());
    }
    if name == "time.steady_millis" && args.is_empty() {
        return Ok("ku_time_steady_millis()".to_string());
    }
    if name == "fs.read" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.read requires a path"))?;
        return Ok(format!("ku_fs_read({})", c_expr(path)?));
    }
    if name == "fs.write" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.write requires a path"))?;
        let content = args
            .get(1)
            .ok_or_else(|| unsupported("native C fs.write requires content"))?;
        return Ok(format!("ku_fs_write({}, {})", c_expr(path)?, c_expr(content)?));
    }
    if name == "fs.exists" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.exists requires a path"))?;
        return Ok(format!("ku_fs_exists({})", c_expr(path)?));
    }
    if name == "json.stringify" {
        let value = args
            .first()
            .ok_or_else(|| unsupported("native C json.stringify requires a value"))?;
        // A KuValue is already boxed; other types get wrapped into one.
        let arg = if matches!(&value.ty, IrType::Named(n) if n == "__ku_value") {
            c_value_expr(value)?
        } else {
            ku_value_wrap(&value.ty, &c_value_expr(value)?)?
        };
        return Ok(format!("ku_json_stringify({})", arg));
    }
    if name == "json.parse" {
        let text = args
            .first()
            .ok_or_else(|| unsupported("native C json.parse requires a string"))?;
        return Ok(format!("ku_json_parse({})", c_expr(text)?));
    }
    if name == "__ku_value_get" {
        let value = args
            .first()
            .ok_or_else(|| unsupported("native C __ku_value_get requires a value"))?;
        let key = args
            .get(1)
            .ok_or_else(|| unsupported("native C __ku_value_get requires a key"))?;
        return Ok(format!(
            "ku_value_get_result({}, {})",
            c_expr(value)?,
            c_expr(key)?
        ));
    }
    if name == "__ku_value_index" {
        let value = args
            .first()
            .ok_or_else(|| unsupported("native C __ku_value_index requires a value"))?;
        let index = args
            .get(1)
            .ok_or_else(|| unsupported("native C __ku_value_index requires an index"))?;
        return Ok(format!(
            "ku_value_index_result({}, {})",
            c_expr(value)?,
            c_expr(index)?
        ));
    }
    if let Some(method) = name.strip_prefix("string.") {
        return c_string_method_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("pg.") {
        return c_pg_intrinsic_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("redis.") {
        return c_redis_intrinsic_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("mysql.") {
        return c_mysql_intrinsic_expr(method, args);
    }
    if name == "str" {
        let arg = args
            .first()
            .ok_or_else(|| unsupported("str requires one argument"))?;
        // Match the interpreter's `value.to_string()` for each primitive: int in
        // decimal, bool as true/false, a string identity (cloned so the source is
        // left intact — `str(x)` borrows x), null as the literal "null".
        return match &arg.ty {
            IrType::Int => Ok(format!("ku_string_from_int({})", c_expr(arg)?)),
            IrType::Bool => Ok(format!("ku_string_from_bool({})", c_expr(arg)?)),
            IrType::Str => Ok(format!("ku_string_clone({})", c_expr(arg)?)),
            IrType::Null => Ok("ku_string_static((const uint8_t*)\"null\", 4)".to_string()),
            other => Err(unsupported(format!(
                "native C str() is not implemented for {other} yet"
            ))),
        };
    }
    match (name, ty) {
        ("ok", IrType::Result(_)) => {
            let value = args
                .first()
                .ok_or_else(|| unsupported("ok requires one argument"))?;
            Ok(format!(
                "({}){{ true, {}, (KuError){{0}} }}",
                c_type(ty)?,
                c_value_expr(value)?
            ))
        }
        ("err", IrType::Result(inner)) => {
            let value = args
                .first()
                .ok_or_else(|| unsupported("err requires one argument"))?;
            Ok(format!(
                "({}){{ false, {}, {} }}",
                c_type(ty)?,
                c_zero_value(inner)?,
                c_error_expr(value)?
            ))
        }
        _ => Err(unsupported(format!(
            "native C prototype cannot lower intrinsic '{name}'"
        ))),
    }
}

fn c_zero_value(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int | IrType::Null => Ok("0".to_string()),
        IrType::Bool => Ok("false".to_string()),
        IrType::Str => Ok("(KuString){0}".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok("NULL".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("ku_v_null()".to_string()),
        IrType::Named(name) if name == "__ku_http_server" => Ok("NULL".to_string()),
        IrType::Named(name) if pg_native_suffix(name).is_some() => Ok("NULL".to_string()),
        IrType::Array(_) | IrType::Named(_) | IrType::Result(_) | IrType::Closure { .. } => {
            Ok(format!("({}){{0}}", c_type(ty)?))
        }
        IrType::Cell(_) => Ok("NULL".to_string()),
        _ => Err(unsupported(format!(
            "native C prototype does not support zero value for {ty}"
        ))),
    }
}

fn is_native_zero(expr: &IrExpr) -> bool {
    matches!(&expr.kind, IrExprKind::Literal(value) if value == "<native-zero>")
}

fn c_zero_initializer(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int | IrType::Null => Ok("0".to_string()),
        IrType::Bool => Ok("false".to_string()),
        IrType::Str => Ok("(KuString){0}".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok("NULL".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("ku_v_null()".to_string()),
        IrType::Named(name) if name == "__ku_http_server" => Ok("NULL".to_string()),
        IrType::Named(name) if pg_native_suffix(name).is_some() => Ok("NULL".to_string()),
        IrType::Array(_) | IrType::Named(_) | IrType::Result(_) | IrType::Closure { .. } => {
            Ok(format!("({}){{0}}", c_type(ty)?))
        }
        IrType::Cell(_) => Ok("NULL".to_string()),
        _ => Err(unsupported(format!(
            "native C prototype does not support zero initialization for {ty}"
        ))),
    }
}

fn c_binary(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Remainder => "%",
        BinaryOp::Equal => "==",
        BinaryOp::NotEqual => "!=",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

/// Move an owned value out of the place `place` (a C lvalue), clearing the
/// source so it is not freed twice. Mirrors the per-kind move helpers used by
/// `c_value_expr`; copy types return the place unchanged.
fn c_move_value(ty: &IrType, place: &str) -> KuResult<String> {
    match ty {
        IrType::Str => Ok(format!("ku_string_move(&{place})")),
        IrType::Array(element) => {
            Ok(format!("ku_array_move_{}(&{place})", c_type_suffix(element)?))
        }
        IrType::Result(inner) => {
            Ok(format!("ku_result_move_{}(&{place})", c_type_suffix(inner)?))
        }
        IrType::Named(name) if name == "__ku_object" => Ok(format!("ku_object_move(&{place})")),
        IrType::Named(name) if name == "__ku_value" => Ok(format!("ku_value_move(&{place})")),
        IrType::Named(name) if name == "__ku_http_server" => Ok(place.to_string()),
        IrType::Named(name) if name == "__ku_error_type" => Ok(format!("ku_error_move(&{place})")),
        IrType::Named(name) => Ok(format!("{}(&{place})", c_named_move_function(name))),
        IrType::Closure { params, ret } => Ok(format!(
            "ku_closure_move_{}(&{place})",
            closure_signature_suffix(params, ret)?
        )),
        IrType::Int | IrType::Bool | IrType::Null => Ok(place.to_string()),
        _ => Err(unsupported(format!(
            "native C move is not implemented for {ty}"
        ))),
    }
}

fn c_clone_value(ty: &IrType, expression: &str) -> KuResult<String> {
    match ty {
        IrType::Array(element) => Ok(format!(
            "ku_array_clone_{}({expression})",
            c_type_suffix(element)?
        )),
        IrType::Named(name) if name == "__ku_object" => {
            Ok(format!("ku_object_clone({expression})"))
        }
        IrType::Named(name) if name == "__ku_value" => {
            Ok(format!("ku_value_clone({expression})"))
        }
        // Stage 8a: the server value is a shared heap pointer, cloned by copy.
        IrType::Named(name) if name == "__ku_http_server" => Ok(expression.to_string()),
        IrType::Named(name) if name != "__ku_error_type" => {
            Ok(format!("{}({expression})", c_named_clone_function(name)))
        }
        IrType::Str => Ok(format!("ku_string_clone({expression})")),
        // Stage 6e: cloning a stored closure shares its captured environment by
        // bumping the env refcount (env==NULL for a Stage 6a no-capture closure
        // makes this a plain struct copy).
        IrType::Closure { params, ret } => Ok(format!(
            "ku_closure_clone_{}({expression})",
            closure_signature_suffix(params, ret)?
        )),
        IrType::Int | IrType::Bool | IrType::Null => Ok(expression.to_string()),
        _ => Err(unsupported(format!(
            "native C clone is not implemented for {ty}"
        ))),
    }
}

fn c_drop_value(ty: &IrType, expression: &str) -> KuResult<String> {
    match ty {
        IrType::Array(element) => Ok(format!(
            "ku_array_drop_{}(&{expression});",
            c_type_suffix(element)?
        )),
        IrType::Named(name) if name == "__ku_value" => {
            Ok(format!("ku_value_drop(&{expression});"))
        }
        // Stage 8a: the server outlives every local (freed by the accept loop on
        // exit, or reclaimed by the OS), so a local going out of scope drops nothing.
        IrType::Named(name) if name == "__ku_http_server" => Ok(String::new()),
        IrType::Named(name) if name != "__ku_error_type" => {
            Ok(format!("{}(&{expression});", c_named_drop_function(name)))
        }
        IrType::Str => Ok(format!("ku_string_drop(&{expression});")),
        // Stage 6e: a stored closure owns a reference to its captured env; drop
        // releases it (env==NULL for a Stage 6a no-capture closure is a no-op).
        IrType::Closure { .. } => Ok(format!(
            "if (({expression}).env) ((KuEnvHeader*)({expression}).env)->release(({expression}).env);"
        )),
        IrType::Int | IrType::Bool | IrType::Null => Ok(String::new()),
        _ => Err(unsupported(format!(
            "native C drop is not implemented for {ty}"
        ))),
    }
}

fn c_unary(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Negate => "-",
        UnaryOp::Not => "!",
    }
}

fn c_ident(name: &str) -> String {
    let mut output = String::new();
    for (index, ch) in name.chars().enumerate() {
        if (index == 0 && (ch.is_ascii_alphabetic() || ch == '_'))
            || (index > 0 && (ch.is_ascii_alphanumeric() || ch == '_'))
        {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "_".to_string()
    } else {
        output
    }
}

fn c_symbol(name: &str) -> String {
    if name == "main" {
        "ku_main".to_string()
    } else {
        c_ident(name)
    }
}

fn c_struct_type(name: &str) -> String {
    format!("KuStruct_{}", c_ident(name))
}

fn c_enum_type(name: &str) -> String {
    format!("KuEnum_{}", c_ident(name))
}

fn c_array_type(element: &IrType) -> KuResult<String> {
    Ok(format!("KuArray_{}", c_type_suffix(element)?))
}

fn c_type_suffix(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int => Ok("int".to_string()),
        IrType::Bool => Ok("bool".to_string()),
        IrType::Str => Ok("str".to_string()),
        IrType::Null => Ok("null".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("kuvalue".to_string()),
        IrType::Named(name) if name == "__ku_http_server" => Ok("http_server".to_string()),
        IrType::Named(name) if pg_native_suffix(name).is_some() => {
            Ok(pg_native_suffix(name).unwrap().to_string())
        }
        IrType::Named(name) => Ok(match enum_type_name(name) {
            Some(name) => format!("enum_{}", c_ident(name)),
            None => format!("struct_{}", c_ident(name)),
        }),
        IrType::Array(inner) => Ok(format!("array_{}", c_type_suffix(inner)?)),
        // Stage 8a: a closure returning `Result<T>` (e.g. an HTTP handler) needs a
        // stable suffix for its ABI struct name.
        IrType::Result(inner) => Ok(format!("result_{}", c_type_suffix(inner)?)),
        IrType::Closure { params, ret } => closure_signature_suffix(params, ret),
        IrType::Cell(inner) => Ok(format!("cell_{}", c_type_suffix(inner)?)),
        _ => Err(unsupported(format!(
            "native C prototype does not support arrays of {ty}"
        ))),
    }
}

fn enum_type_name(name: &str) -> Option<&str> {
    name.strip_prefix("__ku_enum_type:")
}

fn unsupported(message: impl Into<String>) -> KuError {
    KuError::runtime(message.into(), Span::default())
}

fn is_c_owned_type(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Str
            | IrType::Array(_)
            | IrType::Result(_)
            | IrType::Named(_)
            | IrType::Closure { .. }
    )
}

fn collect_owned_locals(function: &IrFunction) -> Vec<OwnedLocal> {
    let mut locals = Vec::new();
    for param in &function.params {
        if is_c_owned_type(&param.ty) {
            locals.push(OwnedLocal {
                source_name: param.name.clone(),
                name: c_ident(&param.name),
                ty: param.ty.clone(),
                is_param: true,
                borrowed: false,
            });
        }
    }
    for block in &function.blocks {
        for instruction in &block.instructions {
            match instruction {
                IrInst::Temp { id, ty, value } if is_c_owned_type(ty) => locals.push(OwnedLocal {
                    source_name: format!("%t{}", id.0),
                    name: format!("t{}", id.0),
                    ty: ty.clone(),
                    is_param: false,
                    // Reading an element out of a container only yields a shallow
                    // alias when the source was NOT cleared. A struct-field read is
                    // now a move-and-clear (`c_move_place`): it clears the field, so
                    // the temp is the sole owner and MUST be dropped (the struct's
                    // own deep-drop will skip the cleared field). Only reads the
                    // backend leaves as shallow copies — array/object index reads —
                    // are true aliases whose drop must be skipped.
                    borrowed: matches!(
                        value.kind,
                        IrExprKind::Index { .. } | IrExprKind::Field { .. }
                    ) && c_move_place(value).ok().flatten().is_none(),
                }),
                IrInst::BindOk { id, ty, .. } if is_c_owned_type(ty) => locals.push(OwnedLocal {
                    source_name: format!("%t{}", id.0),
                    name: format!("t{}", id.0),
                    ty: ty.clone(),
                    is_param: false,
                    borrowed: false,
                }),
                IrInst::Let { name, ty, .. }
                    if is_c_owned_type(ty)
                        && !locals.iter().any(|local| local.source_name == *name) =>
                {
                    locals.push(OwnedLocal {
                        source_name: name.clone(),
                        name: c_ident(name),
                        ty: ty.clone(),
                        is_param: false,
                        borrowed: false,
                    });
                }
                // catch(err) binds an owned Error; drop it at scope exit.
                IrInst::BindError { name, .. }
                    if !locals.iter().any(|local| local.source_name == *name) =>
                {
                    locals.push(OwnedLocal {
                        source_name: name.clone(),
                        name: c_ident(name),
                        ty: IrType::Named("__ku_error_type".to_string()),
                        is_param: false,
                        borrowed: false,
                    });
                }
                // Stage 6b: a boxed local. Pre-declared NULL at the function head
                // and released (once) on every exit path via the owned cleanup.
                IrInst::CellNew { name, ty, .. }
                    if !locals.iter().any(|local| local.source_name == *name) =>
                {
                    locals.push(OwnedLocal {
                        source_name: name.clone(),
                        name: c_ident(name),
                        ty: IrType::Cell(Box::new(ty.clone())),
                        is_param: false,
                        borrowed: false,
                    });
                }
                _ => {}
            }
        }
    }
    locals
}

fn emit_owned_cleanup(out: &mut String, locals: &[OwnedLocal]) -> KuResult<()> {
    for local in locals.iter().rev() {
        if local.borrowed {
            continue;
        }
        emit_drop_expr(out, &local.ty, &local.name)?;
    }
    Ok(())
}

fn emit_drop_expr(out: &mut String, ty: &IrType, expression: &str) -> KuResult<()> {
    match ty {
        IrType::Str => {
            out.push_str(&format!("  ku_string_drop(&{});\n", expression));
            Ok(())
        }
        IrType::Array(element) => {
            out.push_str(&format!(
                "  ku_array_drop_{}(&{});\n",
                c_type_suffix(element)?,
                expression
            ));
            Ok(())
        }
        IrType::Named(name) if name == "__ku_error_type" => {
            out.push_str(&format!("  ku_error_drop(&{});\n", expression));
            Ok(())
        }
        IrType::Named(name) if name == "__ku_object" => {
            out.push_str(&format!("  ku_object_drop({expression});\n"));
            out.push_str(&format!("  {expression} = NULL;\n"));
            Ok(())
        }
        IrType::Named(name) if name == "__ku_value" => {
            out.push_str(&format!("  ku_value_drop(&{expression});\n"));
            Ok(())
        }
        // Stage 8a: the HTTP server outlives every local (freed by the accept loop,
        // or reclaimed at process exit), so a local scope drop is a no-op.
        IrType::Named(name) if name == "__ku_http_server" => Ok(()),
        IrType::Named(name) => {
            out.push_str(&format!(
                "  {}(&{});\n",
                c_named_drop_function(name),
                expression
            ));
            Ok(())
        }
        IrType::Result(inner) => {
            out.push_str(&format!(
                "  ku_result_drop_{}(&{});\n",
                c_type_suffix(inner)?,
                expression
            ));
            Ok(())
        }
        IrType::Closure { .. } => {
            // Stage 6b: release this owner's env reference (type-erased through the
            // env header). A NULL env (no captures, or a moved-from closure) is a
            // no-op. env release cascades into releasing each captured cell.
            out.push_str(&format!(
                "  if (({expression}).env) ((KuEnvHeader*)({expression}).env)->release(({expression}).env);\n"
            ));
            Ok(())
        }
        IrType::Cell(inner) => {
            out.push_str(&format!(
                "  ku_cell_{}_release({expression});\n",
                c_type_suffix(inner)?
            ));
            Ok(())
        }
        _ => Err(unsupported(format!(
            "native C drop is not implemented for {ty}"
        ))),
    }
}

/// A field whose type carries no ownership (a plain bitwise copy) needs no clone
/// or drop.
fn is_copy_ir_type(ty: &IrType) -> bool {
    matches!(ty, IrType::Int | IrType::Bool | IrType::Null)
}

fn emit_named_ownership_helpers(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let has_any =
        !program.layouts.structs.is_empty() || !program.layouts.enums.is_empty();
    // Forward-declare every clone/drop/move first: a struct/enum can hold a field
    // of another user struct/enum, so its deep clone/drop calls that type's
    // clone/drop, which may be defined later.
    for layout in &program.layouts.structs {
        emit_named_ownership_prototypes(out, &layout.name)?;
    }
    for layout in &program.layouts.enums {
        emit_named_ownership_prototypes(out, &format!("__ku_enum_type:{}", layout.name))?;
    }
    for layout in &program.layouts.structs {
        emit_struct_ownership_helper(out, layout)?;
    }
    for layout in &program.layouts.enums {
        emit_enum_ownership_helper(out, layout)?;
    }
    if has_any {
        out.push('\n');
    }
    Ok(())
}

fn emit_named_ownership_prototypes(out: &mut String, name: &str) -> KuResult<()> {
    let c_ty = c_type(&IrType::Named(name.to_string()))?;
    out.push_str(&format!(
        "static {c_ty} {}({c_ty}* value);\nstatic {c_ty} {}({c_ty} value);\nstatic void {}({c_ty}* value);\n",
        c_named_move_function(name),
        c_named_clone_function(name),
        c_named_drop_function(name),
    ));
    Ok(())
}

fn emit_struct_ownership_helper(out: &mut String, layout: &IrStructLayout) -> KuResult<()> {
    let name = &layout.name;
    let c_ty = c_type(&IrType::Named(name.clone()))?;
    let move_fn = c_named_move_function(name);
    let clone_fn = c_named_clone_function(name);
    let drop_fn = c_named_drop_function(name);
    let mut clone_body = String::new();
    let mut drop_body = String::new();
    for field in &layout.fields {
        if is_copy_ir_type(&field.ty) {
            continue;
        }
        let f = c_ident(&field.name);
        clone_body.push_str(&format!(
            " value.{f} = {};",
            c_clone_value(&field.ty, &format!("value.{f}"))?
        ));
        drop_body.push_str(&format!(
            " {}",
            c_drop_value(&field.ty, &format!("value->{f}"))?
        ));
    }
    out.push_str(&format!(
        "static {c_ty} {move_fn}({c_ty}* value) {{ {c_ty} result = *value; *value = ({c_ty}){{0}}; return result; }}\n\
         static {c_ty} {clone_fn}({c_ty} value) {{{clone_body} return value; }}\n\
         static void {drop_fn}({c_ty}* value) {{ if (!value) return;{drop_body} *value = ({c_ty}){{0}}; }}\n"
    ));
    Ok(())
}

fn emit_enum_ownership_helper(out: &mut String, layout: &IrEnumLayout) -> KuResult<()> {
    let name = format!("__ku_enum_type:{}", layout.name);
    let c_ty = c_type(&IrType::Named(name.clone()))?;
    let move_fn = c_named_move_function(&name);
    let clone_fn = c_named_clone_function(&name);
    let drop_fn = c_named_drop_function(&name);
    let mut clone_arms = String::new();
    let mut drop_arms = String::new();
    for variant in &layout.variants {
        let owned: Vec<&IrFieldLayout> = variant
            .fields
            .iter()
            .filter(|field| !is_copy_ir_type(&field.ty))
            .collect();
        if owned.is_empty() {
            continue;
        }
        let v = c_ident(&variant.name);
        let mut clone_body = String::new();
        let mut drop_body = String::new();
        for field in owned {
            let f = c_ident(&field.name);
            clone_body.push_str(&format!(
                " value.payload.{v}.{f} = {};",
                c_clone_value(&field.ty, &format!("value.payload.{v}.{f}"))?
            ));
            drop_body.push_str(&format!(
                " {}",
                c_drop_value(&field.ty, &format!("value->payload.{v}.{f}"))?
            ));
        }
        clone_arms.push_str(&format!(" case {}: {{{clone_body} }} break;", variant.tag));
        drop_arms.push_str(&format!(" case {}: {{{drop_body} }} break;", variant.tag));
    }
    let clone_switch = if clone_arms.is_empty() {
        String::new()
    } else {
        format!(" switch (value.tag) {{{clone_arms} default: break; }}")
    };
    let drop_switch = if drop_arms.is_empty() {
        String::new()
    } else {
        format!(" switch (value->tag) {{{drop_arms} default: break; }}")
    };
    out.push_str(&format!(
        "static {c_ty} {move_fn}({c_ty}* value) {{ {c_ty} result = *value; memset(value, 0, sizeof(*value)); value->tag = -1; return result; }}\n\
         static {c_ty} {clone_fn}({c_ty} value) {{{clone_switch} return value; }}\n\
         static void {drop_fn}({c_ty}* value) {{ if (!value) return;{drop_switch} memset(value, 0, sizeof(*value)); value->tag = -1; }}\n"
    ));
    Ok(())
}

#[allow(dead_code)]
fn emit_named_ownership_helper(out: &mut String, name: &str, is_enum: bool) -> KuResult<()> {
    let ty = IrType::Named(name.to_string());
    let c_ty = c_type(&ty)?;
    let move_fn = c_named_move_function(name);
    let clone_fn = c_named_clone_function(name);
    let drop_fn = c_named_drop_function(name);
    if is_enum {
        out.push_str(&format!(
            "static {c_ty} {move_fn}({c_ty}* value) {{ {c_ty} result = *value; memset(value, 0, sizeof(*value)); value->tag = -1; return result; }}\n\
             static {c_ty} {clone_fn}({c_ty} value) {{ return value; }}\n\
             static void {drop_fn}({c_ty}* value) {{ if (value) {{ memset(value, 0, sizeof(*value)); value->tag = -1; }} }}\n"
        ));
    } else {
        out.push_str(&format!(
            "static {c_ty} {move_fn}({c_ty}* value) {{ {c_ty} result = *value; *value = ({c_ty}){{0}}; return result; }}\n\
             static {c_ty} {clone_fn}({c_ty} value) {{ return value; }}\n\
             static void {drop_fn}({c_ty}* value) {{ if (value) *value = ({c_ty}){{0}}; }}\n"
        ));
    }
    Ok(())
}

/// A C-library binding's opaque handle (`pg`/`redis`) uses its own move/clone/drop
/// helpers (which close the underlying resource), so give it a distinct suffix that
/// routes the generic Named ownership dispatch to `ku_move_pg_conn` /
/// `ku_drop_redis_conn` etc.
fn pg_native_suffix(name: &str) -> Option<&'static str> {
    match name {
        "__ku_pg_conn" => Some("pg_conn"),
        "__ku_pg_result" => Some("pg_result"),
        "__ku_pg_pool" => Some("pg_pool"),
        "__ku_redis_conn" => Some("redis_conn"),
        "__ku_mysql_conn" => Some("mysql_conn"),
        "__ku_mysql_result" => Some("mysql_result"),
        _ => None,
    }
}

fn c_named_suffix(name: &str) -> String {
    if let Some(suffix) = pg_native_suffix(name) {
        return suffix.to_string();
    }
    match enum_type_name(name) {
        Some(name) => format!("enum_{}", c_ident(name)),
        None => format!("struct_{}", c_ident(name)),
    }
}

fn c_named_move_function(name: &str) -> String {
    format!("ku_move_{}", c_named_suffix(name))
}

fn c_named_clone_function(name: &str) -> String {
    format!("ku_clone_{}", c_named_suffix(name))
}

fn c_named_drop_function(name: &str) -> String {
    format!("ku_drop_{}", c_named_suffix(name))
}

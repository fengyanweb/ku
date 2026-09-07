use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

#[path = "c_output.rs"]
mod output;
use output::COutput;

#[path = "c_task.rs"]
mod task;

// Whole generated-file bytes, including shared runtimes and all specializations.
// This is independent of the checker's generic AST/type admission budget.
const MAX_GENERATED_C_BYTES: usize = 64 * 1024 * 1024;

use crate::{
    ast::{BinaryOp, ParamMode, UnaryOp},
    error::{KuError, KuResult},
    ir::{
        FunctionId, IrBlock, IrCallKind, IrCaptureSource, IrEnumLayout, IrExpr, IrExprKind,
        IrFieldLayout, IrFunction, IrInst, IrLValue, IrProgram, IrStructLayout, IrTerminator,
        IrType,
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
fn closure_signature_suffix(
    params: &[IrType],
    param_modes: &[ParamMode],
    ret: &IrType,
) -> KuResult<String> {
    if params.len() != param_modes.len() {
        return Err(unsupported(
            "function parameter mode count does not match signature",
        ));
    }
    let mut suffix = String::from("fn");
    for (param, mode) in params.iter().zip(param_modes) {
        suffix.push('_');
        if *mode == ParamMode::View {
            suffix.push_str("view_");
        }
        suffix.push_str(&c_type_suffix(param)?);
    }
    suffix.push_str("__to_");
    suffix.push_str(&c_type_suffix(ret)?);
    Ok(suffix)
}

fn c_param_type(ty: &IrType, mode: ParamMode) -> KuResult<String> {
    let value = c_type(ty)?;
    if mode == ParamMode::View && is_c_owned_type(ty) {
        Ok(if value.ends_with('*') {
            format!("{value} const*")
        } else {
            format!("const {value}*")
        })
    } else {
        Ok(value)
    }
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

#[derive(Clone)]
struct ForEachState {
    block_id: crate::ir::BlockId,
    after_block: crate::ir::BlockId,
    name: String,
    iterable_ty: IrType,
    element_ty: IrType,
}

fn for_state_prefix(block_id: crate::ir::BlockId) -> String {
    format!("__ku_for_{}", block_id.0)
}

fn collect_for_each_states(function: &IrFunction) -> KuResult<Vec<ForEachState>> {
    let mut states = Vec::new();
    let mut source_bindings = function
        .params
        .iter()
        .map(|param| param.name.clone())
        .collect::<HashSet<_>>();
    for block in &function.blocks {
        for inst in &block.instructions {
            match inst {
                IrInst::Let { name, .. }
                | IrInst::CellNew { name, .. }
                | IrInst::BindError { name, .. } => {
                    source_bindings.insert(name.clone());
                }
                _ => {}
            }
        }
    }
    let mut loop_bindings = HashSet::new();
    for block in &function.blocks {
        let IrTerminator::ForEach {
            name,
            iterable,
            after_block,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if source_bindings.contains(name) || !loop_bindings.insert(name.clone()) {
            return Err(unsupported(format!(
                "native C for loop variable '{name}' cannot shadow another local or loop variable yet"
            )));
        }
        let element_ty = match &iterable.ty {
            IrType::Int => IrType::Int,
            IrType::Array(element) => (**element).clone(),
            other => {
                return Err(unsupported(format!(
                    "native C for expects array or int but got {other}"
                )));
            }
        };
        states.push(ForEachState {
            block_id: block.id,
            after_block: *after_block,
            name: name.clone(),
            iterable_ty: iterable.ty.clone(),
            element_ty,
        });
    }
    let c_bindings = source_bindings
        .iter()
        .chain(loop_bindings.iter())
        .map(|name| c_ident(name))
        .collect::<HashSet<_>>();
    for state in &states {
        let prefix = for_state_prefix(state.block_id);
        let mut generated = vec![format!("{prefix}_initialized"), format!("{prefix}_index")];
        if state.iterable_ty == IrType::Int {
            generated.push(format!("{prefix}_limit"));
        } else {
            generated.push(format!("{prefix}_array"));
        }
        if let Some(name) = generated.into_iter().find(|name| c_bindings.contains(name)) {
            return Err(unsupported(format!(
                "native C for loop internal name '{name}' conflicts with a user binding"
            )));
        }
    }
    Ok(states)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeFsBase {
    /// Preserve the historical backend API: relative paths are interpreted by
    /// the process working directory.
    CurrentWorkingDirectory,
    /// Resolve relative paths from a relocatable locator anchored at the native
    /// executable's directory.
    ExecutableRelative(String),
    /// The CLI could not represent a relocatable source locator. This is only an
    /// error when the lowered program actually calls std.fs.
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CBackendOptions {
    pub fs_base: NativeFsBase,
    /// Compile deterministic object-ABI allocation failure sites into this C
    /// artifact. Production/default artifacts keep this false and lower the
    /// allocation shims directly to malloc/calloc/realloc.
    pub object_oom_fault_injection: bool,
}

impl Default for CBackendOptions {
    fn default() -> Self {
        Self {
            fs_base: NativeFsBase::CurrentWorkingDirectory,
            object_oom_fault_injection: false,
        }
    }
}

pub fn generate_c_source(program: &IrProgram) -> KuResult<String> {
    generate_c_source_with_options(program, &CBackendOptions::default())
}

pub fn generate_c_source_with_options(
    program: &IrProgram,
    options: &CBackendOptions,
) -> KuResult<String> {
    generate_c_source_bounded(program, options, MAX_GENERATED_C_BYTES)
}

/// Compile verified internal task frames alongside the existing synchronous
/// runtime helpers. This is not AST async lowering and is not a CLI capability.
/// In particular it supplies no scheduler, Task handle or external I/O runtime.
pub fn generate_task_frame_c_source(
    program: &IrProgram,
    frames: &crate::ir::task::TaskProgram,
) -> KuResult<String> {
    let plan = crate::ir::task::verify_and_plan(frames, Default::default())?;
    if !frames.functions.is_empty()
        && program.functions.iter().any(|function| {
            let name = c_symbol(&function.name);
            name.starts_with("ku_task_frame_")
                || name.starts_with("KuTaskFrame")
                || name.starts_with("KU_TASK_FRAME_")
        })
    {
        return Err(unsupported(
            "synchronous function collides with internal task frame ABI",
        ));
    }
    generate_c_source_with_frames_bounded(
        program,
        &CBackendOptions::default(),
        MAX_GENERATED_C_BYTES,
        Some((frames, &plan)),
    )
}

fn generate_c_source_bounded(
    program: &IrProgram,
    options: &CBackendOptions,
    byte_limit: usize,
) -> KuResult<String> {
    generate_c_source_with_frames_bounded(program, options, byte_limit, None)
}

fn generate_c_source_with_frames_bounded(
    program: &IrProgram,
    options: &CBackendOptions,
    byte_limit: usize,
    frames: Option<(
        &crate::ir::task::TaskProgram,
        &crate::ir::task::TaskFramePlan,
    )>,
) -> KuResult<String> {
    crate::ir::verify_borrow_contract(program)?;
    let mut frame_result_types = Vec::new();
    if let Some((frames, _)) = frames {
        for function in &frames.functions {
            collect_result_type(&function.result, &mut frame_result_types)?;
            for slot in &function.slots {
                let crate::ir::task::TaskSlotType::Value { ty, .. } = &slot.ty;
                collect_result_type(ty, &mut frame_result_types)?;
            }
        }
    }
    for function in &program.functions {
        validate_cfg(function)?;
    }
    validate_layouts(program)?;
    let fs_usage = program_fs_usage(program);
    if fs_usage.any() {
        if let NativeFsBase::Unavailable(reason) = &options.fs_base {
            return Err(unsupported(format!(
                "native fs source base is unavailable: {reason}"
            )));
        }
    }
    let mut out = COutput::new(byte_limit);
    out.push_str(
        "#if defined(__linux__) && !defined(_GNU_SOURCE)\n#define _GNU_SOURCE\n#endif\n\
         #if defined(__APPLE__) && !defined(_DARWIN_C_SOURCE)\n#define _DARWIN_C_SOURCE\n#endif\n\
         #if !defined(_WIN32) && !defined(_POSIX_C_SOURCE)\n#define _POSIX_C_SOURCE 200809L\n#endif\n\
         #include <stdbool.h>\n#include <stdint.h>\n#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n#include <time.h>\n#include <errno.h>\n#include <limits.h>\n#include <math.h>\n\n\
         #if defined(_WIN32)\n\
         #if defined(_MSC_VER)\n__declspec(dllimport) unsigned long long __stdcall GetTickCount64(void);\n\
         #elif defined(__GNUC__) || defined(__clang__)\n__attribute__((dllimport)) unsigned long long __attribute__((stdcall)) GetTickCount64(void);\n\
         #else\nunsigned long long GetTickCount64(void);\n#endif\n\
         #endif\n\n\
         typedef struct KuString {\n  uint8_t* ptr;\n  size_t len;\n  size_t capacity;\n  uint8_t storage;\n} KuString;\n\
         enum { KU_STRING_STATIC = 0, KU_STRING_OWNED = 1 };\n\
         static KuString ku_string_static(const uint8_t* ptr, size_t len) {\n  return (KuString){ (uint8_t*)ptr, len, 0, KU_STRING_STATIC };\n}\n\
         static KuString ku_string_clone(KuString value) {\n  if (value.storage == KU_STRING_STATIC) return value;\n  if (!value.ptr) return (KuString){0};\n  size_t capacity = value.len ? value.len : 1;\n  uint8_t* data = (uint8_t*)malloc(capacity);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  if (value.len) memcpy(data, value.ptr, value.len);\n  return (KuString){ data, value.len, capacity, KU_STRING_OWNED };\n}\n\
         static KuString ku_string_move(KuString* value) {\n  KuString moved = *value;\n  *value = (KuString){0};\n  return moved;\n}\n\
         static void ku_string_drop(KuString* value) {\n  if (!value) return;\n  if (value->storage == KU_STRING_OWNED && value->ptr) free(value->ptr);\n  *value = (KuString){0};\n}\n\
         static void ku_string_write(FILE* stream, KuString value) {\n  if (!stream || !value.ptr || value.len == 0) return;\n  size_t offset = 0;\n  while (offset < value.len) {\n    size_t written = fwrite(value.ptr + offset, 1, value.len - offset, stream);\n    if (written == 0) {\n      if (stream != stderr) fputs(\"output write failed\\n\", stderr);\n      exit(1);\n    }\n    offset += written;\n  }\n}\n\
         static size_t ku_size_add(size_t left, size_t right, const char* what) {\n  if (right > SIZE_MAX - left) { fprintf(stderr, \"%s is too large\\n\", what); exit(1); }\n  return left + right;\n}\n\
         static size_t ku_size_mul(size_t left, size_t right, const char* what) {\n  if (left != 0 && right > SIZE_MAX / left) { fprintf(stderr, \"%s is too large\\n\", what); exit(1); }\n  return left * right;\n}\n\
         static size_t ku_collection_capacity(size_t current, size_t required, size_t element_size, const char* what) {\n  size_t limit = SIZE_MAX / element_size;\n  if (required > limit || current > limit) { fprintf(stderr, \"%s is too large\\n\", what); exit(1); }\n  size_t capacity = current ? current : (limit < 8 ? limit : 8);\n  while (capacity < required) capacity = capacity > limit / 2 ? limit : capacity * 2;\n  return capacity;\n}\n\
         static bool ku_string_equal(KuString left, KuString right) {\n  return left.len == right.len && (left.len == 0 || memcmp(left.ptr, right.ptr, left.len) == 0);\n}\n\
         static KuString ku_string_concat(KuString left, KuString right) {\n  size_t len = ku_size_add(left.len, right.len, \"string allocation\");\n  uint8_t* data = (uint8_t*)malloc(len ? len : 1);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  if (left.len) memcpy(data, left.ptr, left.len);\n  if (right.len) memcpy(data + left.len, right.ptr, right.len);\n  return (KuString){ data, len, len, KU_STRING_OWNED };\n}\n\
         /* Only lowered local str += uses this helper. Its RHS is already fully\n            evaluated into a separate owner; no user callback runs during reuse. */\n\
         static KuString ku_string_concat_reuse(KuString* source, KuString right) {\n  if (right.len == 0) return ku_string_move(source);\n  size_t len = ku_size_add(source->len, right.len, \"string allocation\");\n  KuString result = *source;\n  size_t capacity = result.capacity > result.len ? result.capacity : result.len;\n  if (result.storage != KU_STRING_OWNED || len > capacity) {\n    capacity = ku_collection_capacity(capacity, len, 1, \"string allocation\");\n    uint8_t* data = result.storage == KU_STRING_OWNED ? (uint8_t*)realloc(result.ptr, capacity) : (uint8_t*)malloc(capacity);\n    if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n    if (result.storage != KU_STRING_OWNED && result.len) memcpy(data, result.ptr, result.len);\n    result.ptr = data;\n    result.storage = KU_STRING_OWNED;\n  }\n  memcpy(result.ptr + result.len, right.ptr, right.len);\n  result.len = len;\n  result.capacity = capacity;\n  *source = (KuString){0};\n  return result;\n}\n\
         static char* ku_string_to_cstr(KuString value) {\n  size_t capacity = ku_size_add(value.len, 1, \"C string allocation\");\n  char* data = (char*)malloc(capacity);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  if (value.len) memcpy(data, value.ptr, value.len);\n  data[value.len] = '\\0';\n  return data;\n}\n\
         static KuString ku_string_from_int(int64_t value) {\n  char buf[24];\n  int n = snprintf(buf, sizeof(buf), \"%lld\", (long long)value);\n  if (n <= 0) return (KuString){0};\n  uint8_t* data = (uint8_t*)malloc((size_t)n);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  memcpy(data, buf, (size_t)n);\n  return (KuString){ data, (size_t)n, (size_t)n, KU_STRING_OWNED };\n}\n\
         static KuString ku_string_from_bool(bool value) {\n  return value ? ku_string_static((const uint8_t*)\"true\", 4) : ku_string_static((const uint8_t*)\"false\", 5);\n}\n\
         static size_t ku_string_char_len(KuString s) {\n  size_t count = 0;\n  for (size_t i = 0; i < s.len; i++) { if ((s.ptr[i] & 0xC0) != 0x80) count++; }\n  return count;\n}\n\
         static bool ku_bytes_find(KuString hay, KuString needle, size_t from, size_t* out) {\n  if (from > hay.len) return false;\n  if (needle.len == 0) { *out = from; return true; }\n  if (needle.len > hay.len - from) return false;\n  size_t last = hay.len - needle.len;\n  for (size_t i = from;; i++) {\n    if (memcmp(hay.ptr + i, needle.ptr, needle.len) == 0) { *out = i; return true; }\n    if (i == last) break;\n  }\n  return false;\n}\n\
         static bool ku_string_contains(KuString hay, KuString needle) {\n  size_t at; return ku_bytes_find(hay, needle, 0, &at);\n}\n\
         static bool ku_string_starts_with(KuString s, KuString prefix) {\n  return prefix.len <= s.len && (prefix.len == 0 || memcmp(s.ptr, prefix.ptr, prefix.len) == 0);\n}\n\
         static bool ku_string_ends_with(KuString s, KuString suffix) {\n  return suffix.len <= s.len && (suffix.len == 0 || memcmp(s.ptr + (s.len - suffix.len), suffix.ptr, suffix.len) == 0);\n}\n\
         static KuString ku_string_alloc(size_t len) {\n  if (len == 0) return (KuString){0};\n  uint8_t* data = (uint8_t*)malloc(len);\n  if (!data) { fprintf(stderr, \"out of memory\\n\"); exit(1); }\n  return (KuString){ data, len, len, KU_STRING_OWNED };\n}\n\
         static KuString ku_string_replace(KuString s, KuString from, KuString to) {\n  if (from.len == 0) {\n    /* Count the same byte chunks that the copy loop consumes. This remains safe\n       even if an ABI caller hands us malformed UTF-8. */\n    size_t chunks = 0, scan = 0;\n    while (scan < s.len) {\n      size_t next = scan + 1;\n      while (next < s.len && (s.ptr[next] & 0xC0) == 0x80) next++;\n      chunks++;\n      scan = next;\n    }\n    size_t slots = ku_size_add(chunks, 1, \"string replacement\");\n    size_t inserted = ku_size_mul(to.len, slots, \"string replacement\");\n    size_t out_len = ku_size_add(inserted, s.len, \"string replacement\");\n    KuString out = ku_string_alloc(out_len);\n    size_t o = 0, i = 0;\n    if (to.len) { memcpy(out.ptr + o, to.ptr, to.len); o += to.len; }\n    while (i < s.len) {\n      size_t j = i + 1;\n      while (j < s.len && (s.ptr[j] & 0xC0) == 0x80) j++;\n      memcpy(out.ptr + o, s.ptr + i, j - i); o += j - i;\n      if (to.len) { memcpy(out.ptr + o, to.ptr, to.len); o += to.len; }\n      i = j;\n    }\n    return out;\n  }\n  size_t count = 0, i = 0, at;\n  while (ku_bytes_find(s, from, i, &at)) { count++; i = at + from.len; }\n  if (count == 0) return ku_string_clone(s);\n  size_t removed = ku_size_mul(count, from.len, \"string replacement\");\n  size_t added = ku_size_mul(count, to.len, \"string replacement\");\n  size_t out_len = ku_size_add(s.len - removed, added, \"string replacement\");\n  KuString out = ku_string_alloc(out_len);\n  size_t o = 0, prev = 0;\n  i = 0;\n  while (ku_bytes_find(s, from, i, &at)) {\n    if (at > prev) { memcpy(out.ptr + o, s.ptr + prev, at - prev); o += at - prev; }\n    if (to.len) { memcpy(out.ptr + o, to.ptr, to.len); o += to.len; }\n    prev = at + from.len;\n    i = prev;\n  }\n  if (s.len > prev) { memcpy(out.ptr + o, s.ptr + prev, s.len - prev); o += s.len - prev; }\n  return out;\n}\n\n\
         typedef struct KuError {\n  KuString domain;\n  KuString code;\n  KuString message;\n} KuError;\n\
         static KuError ku_error_make(KuString domain, KuString code, KuString message) {\n  return (KuError){ domain, code, message };\n}\n\
         static KuError ku_error_clone(KuError error) {\n  return (KuError){ ku_string_clone(error.domain), ku_string_clone(error.code), ku_string_clone(error.message) };\n}\n\
         static KuError ku_error_move(KuError* error) {\n  KuError moved = *error;\n  *error = (KuError){0};\n  return moved;\n}\n\
         static void ku_error_drop(KuError* error) {\n  if (!error) return;\n  ku_string_drop(&error->domain);\n  ku_string_drop(&error->code);\n  ku_string_drop(&error->message);\n  *error = (KuError){0};\n}\n\
         typedef struct KuTime {\n  uint64_t tag;\n  int64_t millis;\n} KuTime;\n\
         #define KU_TIME_TAG UINT64_C(0x4b7554696d650001)\n\
         static void ku_time_fail(const char* message) {\n  fprintf(stderr, \"%s\\n\", message);\n  exit(1);\n}\n\
         static int64_t ku_time_timespec_millis(const struct timespec* ts, const char* operation) {\n  if (!ts || ts->tv_nsec < 0 || ts->tv_nsec >= 1000000000L) ku_time_fail(operation);\n  if ((time_t)-1 > (time_t)0 && ts->tv_sec > (time_t)INT64_MAX) ku_time_fail(operation);\n  int64_t seconds = (int64_t)ts->tv_sec;\n  if (seconds > INT64_MAX / 1000 || seconds < INT64_MIN / 1000) ku_time_fail(operation);\n  int64_t millis = seconds * 1000;\n  int64_t fraction = (int64_t)(ts->tv_nsec / 1000000L);\n  if (millis > INT64_MAX - fraction) ku_time_fail(operation);\n  return millis + fraction;\n}\n\
         static int64_t ku_time_now_millis(void) {\n  struct timespec ts = {0};\n  if (timespec_get(&ts, TIME_UTC) != TIME_UTC) ku_time_fail(\"time: wall clock unavailable\");\n  return ku_time_timespec_millis(&ts, \"time: wall clock is outside supported range\");\n}\n\
         static int64_t ku_time_steady_millis(void) {\n#if defined(_WIN32)\n  unsigned long long ticks = GetTickCount64();\n  if (ticks > (unsigned long long)INT64_MAX) ku_time_fail(\"time.steady_millis: clock is outside supported range\");\n  return (int64_t)ticks;\n#else\n  struct timespec ts = {0};\n  if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) ku_time_fail(\"time.steady_millis: monotonic clock unavailable\");\n  return ku_time_timespec_millis(&ts, \"time.steady_millis: clock is outside supported range\");\n#endif\n}\n\
         static KuTime ku_time_instant(void) {\n  return (KuTime){ KU_TIME_TAG, ku_time_now_millis() };\n}\n\
         static void ku_time_validate(KuTime value) {\n  if (value.tag != KU_TIME_TAG) ku_time_fail(\"time: expected Time value\");\n}\n\
         static KuString ku_time_kind(KuTime value) {\n  ku_time_validate(value);\n  return ku_string_static((const uint8_t*)\"time.time\", 9);\n}\n\
         static int64_t ku_time_value_millis(KuTime value) {\n  ku_time_validate(value);\n  return value.millis;\n}\n\
         static bool ku_time_equal(KuTime left, KuTime right) {\n  ku_time_validate(left);\n  ku_time_validate(right);\n  return left.millis == right.millis;\n}\n\
         static int64_t ku_time_elapsed(KuTime previous) {\n  ku_time_validate(previous);\n  int64_t now = ku_time_now_millis();\n  if ((previous.millis > 0 && now < INT64_MIN + previous.millis) ||\n      (previous.millis < 0 && now > INT64_MAX + previous.millis)) {\n    ku_time_fail(\"time.elapsed: elapsed milliseconds overflow\");\n  }\n  return now - previous.millis;\n}\n\
         static void ku_time_print(KuTime value) {\n  ku_time_validate(value);\n  printf(\"{ kind: time.time, millis: %lld }\", (long long)value.millis);\n}\n\n",
    );
    out.check()?;
    // Socket headers shared by the native HTTP and Redis runtimes. `winsock2.h`
    // must precede any `windows.h`; on POSIX both runtimes use poll(2) rather
    // than select(2), so descriptors above FD_SETSIZE remain safe.
    // The pragma makes MSVC link `ws2_32` without a command-line change.
    // The pg connection poller also needs WSAPoll/poll and shutdown(2). Keep a
    // PG-only artifact lean: it does not need the HTTP/Redis resolver, atomic,
    // or thread headers unless it also uses one of those runtimes.
    let uses_native_socket_runtime =
        program_uses_http(program) || program_uses_redis(program) || program_uses_net(program);
    if uses_native_socket_runtime {
        out.push_str(
            "#if defined(_WIN32)\n\
             #ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n\
             #include <winsock2.h>\n#include <ws2tcpip.h>\n#include <process.h>\n\
             #if defined(_MSC_VER)\n#pragma comment(lib, \"ws2_32.lib\")\n#endif\n\
             #else\n\
             #include <sys/types.h>\n#include <sys/socket.h>\n#include <sys/time.h>\n\
             #include <netdb.h>\n#include <netinet/in.h>\n#include <arpa/inet.h>\n\
             #include <unistd.h>\n#include <fcntl.h>\n#include <poll.h>\n#include <pthread.h>\n#include <stdatomic.h>\n\
             #endif\n\n",
        );
    } else if program_uses_pg(program) {
        out.push_str(
            "#if defined(_WIN32)\n\
             #ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n\
             #include <winsock2.h>\n\
             #if defined(_MSC_VER)\n#pragma comment(lib, \"ws2_32.lib\")\n#endif\n\
             #else\n\
             #include <sys/socket.h>\n#include <poll.h>\n\
             #endif\n\n",
        );
    }
    if program_uses_pg_client(program) && !uses_native_socket_runtime {
        out.push_str("#if !defined(_WIN32)\n#include <pthread.h>\n#endif\n\n");
    }
    if program_uses_mysql(program) {
        // winsock2, when needed by another runtime, has already been included.
        out.push_str(
            "#if defined(_WIN32)\n\
             #ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n\
             #include <windows.h>\n\
             #else\n#include <pthread.h>\n#endif\n\n",
        );
    }
    emit_fs_headers(&mut out, fs_usage);
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
    // Cooperative handler cancellation is thread-local: the worker that invokes
    // a route owns both the deadline flag and the socket. Generated Ku functions
    // only poll and follow explicit IR control-flow edges; no watchdog thread can
    // race a response write or close. Timed-out structured cleanup gets a fixed,
    // non-configurable grace window: finite finally blocks can finish, while an
    // infinite finally cannot occupy the worker indefinitely. The inactive
    // (deadline == 0) fast path makes safepoints harmless outside HTTP execution.
    out.push_str(
        "#define KU_HANDLER_CLEANUP_GRACE_MS 1000ULL\n\
         static KU_THREAD_LOCAL unsigned long long __ku_handler_deadline = 0;\n\
         static KU_THREAD_LOCAL unsigned long long __ku_handler_cleanup_deadline = 0;\n\
         static KU_THREAD_LOCAL int __ku_handler_timed_out = 0;\n\
         static KU_THREAD_LOCAL unsigned long __ku_handler_unwind_depth = 0;\n\
         static unsigned long long __ku_handler_now_ms(void) {\n\
         #if defined(_WIN32)\n\
         \x20 return GetTickCount64();\n\
         #else\n\
         \x20 struct timespec ts = {0};\n\
         \x20 if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) { fputs(\"monotonic clock unavailable\\n\", stderr); exit(1); }\n\
         \x20 return (unsigned long long)ts.tv_sec * 1000ULL + (unsigned long long)(ts.tv_nsec / 1000000L);\n\
         #endif\n\
         }\n\
         static void __ku_handler_timeout_begin(unsigned long long timeout_ms) {\n\
         \x20 unsigned long long now = __ku_handler_now_ms();\n\
         \x20 __ku_handler_timed_out = 0;\n\
         \x20 __ku_handler_unwind_depth = 0;\n\
         \x20 __ku_handler_cleanup_deadline = 0;\n\
         \x20 __ku_handler_deadline = (~0ULL - now < timeout_ms) ? ~0ULL : now + timeout_ms;\n\
         }\n\
         static int __ku_handler_timeout_poll(void) {\n\
         \x20 if (!__ku_handler_timed_out && __ku_handler_deadline == 0) return 0;\n\
         \x20 unsigned long long now = __ku_handler_now_ms();\n\
         \x20 if (!__ku_handler_timed_out && __ku_handler_deadline != 0 && now >= __ku_handler_deadline) __ku_handler_timed_out = 1;\n\
         \x20 if (!__ku_handler_timed_out) return 0;\n\
         \x20 if (__ku_handler_unwind_depth == 0) return 1;\n\
         \x20 return __ku_handler_cleanup_deadline != 0 && now >= __ku_handler_cleanup_deadline;\n\
         }\n\
         static void __ku_handler_timeout_enter(void) {\n\
         \x20 if (__ku_handler_cleanup_deadline == 0) {\n\
         \x20   unsigned long long now = __ku_handler_now_ms();\n\
         \x20   __ku_handler_cleanup_deadline = (~0ULL - now < KU_HANDLER_CLEANUP_GRACE_MS) ? ~0ULL : now + KU_HANDLER_CLEANUP_GRACE_MS;\n\
         \x20 }\n\
         \x20 __ku_handler_unwind_depth++;\n\
         }\n\
         static void __ku_handler_timeout_leave(void) { if (__ku_handler_unwind_depth != 0) __ku_handler_unwind_depth--; }\n\
         static int __ku_handler_timeout_finish(void) {\n\
         \x20 if (!__ku_handler_timed_out && __ku_handler_deadline != 0 && __ku_handler_now_ms() >= __ku_handler_deadline) __ku_handler_timed_out = 1;\n\
         \x20 int timed_out = __ku_handler_timed_out;\n\
         \x20 __ku_handler_deadline = 0;\n\
         \x20 __ku_handler_cleanup_deadline = 0;\n\
         \x20 __ku_handler_timed_out = 0;\n\
         \x20 __ku_handler_unwind_depth = 0;\n\
         \x20 return timed_out;\n\
         }\n\n",
    );
    // Aggregate struct fields (e.g. `[Person]`) need a layered emission so the
    // struct↔array cycle resolves: forward-declare every struct tag, then emit all
    // array typedefs (a `KuArray_KuStruct_X` only needs the struct as a pointer),
    // then the struct bodies (which can embed a `KuArray_*` by value), then the
    // ownership helpers (forward-declared so struct-clone↔array-clone can recurse).
    emit_struct_forward_decls(&mut out, program);
    // Arrays store their element behind a pointer, so a declared (not yet
    // complete) Result/closure tag is enough for `[T!]` and `[fn(...): T]`.
    // Their bodies/helpers are completed later, after their by-value
    // dependencies are available.
    emit_result_forward_decls(&mut out, program, &frame_result_types)?;
    emit_closure_forward_decls(&mut out, program)?;
    emit_array_typedefs(&mut out, program)?;
    emit_array_helper_prototypes(&mut out, program)?;
    // Opaque libpq handle typedefs must precede the Result ABI.
    emit_pg_types(&mut out, program);
    emit_redis_types(&mut out, program);
    emit_mysql_types(&mut out, program);
    emit_bytes_types(&mut out, program);
    emit_net_types(&mut out, program);
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
        emit_closure_refcount_header(&mut out);
        closure_header_done = true;
    }
    // Closure struct typedefs come in two passes around the aggregate ABIs so the
    // cyclic-looking (but acyclic) dependency between array-of-closures and
    // closure-returning-array is resolved by emission order (see `emit_closure_types`).
    let mut closure_emitted = std::collections::HashSet::new();
    emit_closure_types(
        &mut out,
        program,
        &mut closure_header_done,
        &mut closure_emitted,
        true,
    )?;
    emit_object_abi(&mut out, program, options.object_oom_fault_injection)?;
    // Object-valued typed arrays need KuObject/KuValue to be complete before
    // their typed KuArray helper bodies call ku_object_{clone,drop,move}.
    emit_late_array_typedefs(&mut out, program)?;
    emit_http_types(&mut out, program)?;
    emit_result_abi(&mut out, program, &frame_result_types)?;
    emit_closure_types(
        &mut out,
        program,
        &mut closure_header_done,
        &mut closure_emitted,
        false,
    )?;
    // Array helper bodies use element sizeof/clone/drop. Result and aggregate-
    // signature closure elements are complete only at this point. Result helper
    // bodies can already refer to the earlier clone/drop prototypes.
    emit_array_helper_bodies(&mut out, program)?;
    emit_bytes_runtime(&mut out, program);
    emit_windows_socket_runtime(&mut out, program);
    emit_net_runtime(&mut out, program);
    emit_string_chars_helper(&mut out, program)?;
    emit_kuvalue_array_wrappers(&mut out, program)?;
    emit_kuvalue_typed_array_equality_helpers(&mut out, program)?;
    emit_fs_runtime(&mut out, fs_usage, &options.fs_base);
    emit_array_try_get_helpers(&mut out, program)?;
    emit_string_slice_helper(&mut out, program)?;
    emit_object_result_helpers(&mut out, program)?;
    emit_closure_value_wrappers(&mut out, program)?;
    if program_uses_object(program) {
        // JSON returns Result<str> and its typed writers borrow fully-defined
        // arrays/closures/results. Keep it after all of those ABI phases rather
        // than defining functions over incomplete forward declarations.
        emit_json_runtime(&mut out);
        emit_json_typed_stringify_helpers(&mut out, program)?;
    }
    emit_cell_types(&mut out, program)?;
    emit_env_types(&mut out, program)?;
    emit_array_map_helpers(&mut out, program)?;
    emit_function_prototypes(&mut out, program)?;
    emit_closure_thunk_prototypes(&mut out, program)?;
    emit_http_runtime(&mut out, program)?;
    emit_pg_runtime(&mut out, program);
    emit_redis_runtime(&mut out, program);
    emit_mysql_runtime(&mut out, program);
    for function in &program.functions {
        out.check()?;
        emit_function(&mut out, function)?;
        out.push('\n');
    }
    emit_closure_thunks(&mut out, program)?;
    if let Some((frames, plan)) = frames {
        task::emit_frames(&mut out, frames, plan)?;
    }
    emit_main_wrapper(&mut out, program, fs_usage, &options.fs_base)?;
    out.finish()
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
    params.iter().chain(std::iter::once(ret)).all(|ty| {
        matches!(
            ty,
            IrType::Int | IrType::Float | IrType::Bool | IrType::Str | IrType::Null | IrType::Void
        )
    })
}

/// Declare every signature-specific closure tag before any array typedef. An
/// array only stores `KuClosure_*` behind a pointer, so the tag may remain
/// incomplete until its signature's aggregate types are available.
fn emit_closure_forward_decls(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut types = Vec::new();
    collect_closure_types_program(program, &mut types);
    let mut emitted = std::collections::HashSet::new();
    for ty in &types {
        let IrType::Closure {
            params,
            param_modes,
            ret,
        } = ty
        else {
            continue;
        };
        let suffix = closure_signature_suffix(params, param_modes, ret)?;
        if emitted.insert(suffix.clone()) {
            out.push_str(&format!(
                "typedef struct KuClosure_{suffix} KuClosure_{suffix};\n"
            ));
        }
    }
    if !emitted.is_empty() {
        out.push('\n');
    }
    Ok(())
}

/// Emit the type-erased closure environment header and the atomic reference
/// count operations shared by environments and captured-value cells.
///
/// MSVC's C11 mode does not provide a portable `<stdatomic.h>` implementation
/// across every supported Visual Studio release, while the native CLI always
/// drives the 64-bit MSVC toolchain. Use the compiler intrinsic there. GCC,
/// Clang, and Zig use the C11 atomic API over a naturally aligned `size_t`.
/// Retain only needs relaxed ordering; the final release is acquire/release so
/// the unique 1 -> 0 thread observes prior writes before destroying the payload.
fn emit_closure_refcount_header(out: &mut COutput) {
    if out.failed() {
        return;
    }
    out.push_str(
        r#"#if defined(_MSC_VER)
#include <intrin.h>
typedef volatile __int64 KuAtomicRefcount;
#define KU_REFCOUNT_MAX ((size_t)INT64_MAX)
static void ku_atomic_refcount_init(KuAtomicRefcount* counter) { *counter = 1; }
static size_t ku_atomic_refcount_load(KuAtomicRefcount* counter) {
  return (size_t)_InterlockedCompareExchange64(counter, 0, 0);
}
static bool ku_atomic_refcount_compare_exchange_relaxed(
    KuAtomicRefcount* counter, size_t* expected, size_t desired) {
  __int64 observed = _InterlockedCompareExchange64(
      counter, (__int64)desired, (__int64)*expected);
  if ((size_t)observed == *expected) return true;
  *expected = (size_t)observed;
  return false;
}
static bool ku_atomic_refcount_compare_exchange_acq_rel(
    KuAtomicRefcount* counter, size_t* expected, size_t desired) {
  return ku_atomic_refcount_compare_exchange_relaxed(counter, expected, desired);
}
#else
#include <stdatomic.h>
typedef _Atomic size_t KuAtomicRefcount;
#define KU_REFCOUNT_MAX SIZE_MAX
static void ku_atomic_refcount_init(KuAtomicRefcount* counter) {
  atomic_init(counter, (size_t)1);
}
static size_t ku_atomic_refcount_load(KuAtomicRefcount* counter) {
  return atomic_load_explicit(counter, memory_order_relaxed);
}
static bool ku_atomic_refcount_compare_exchange_relaxed(
    KuAtomicRefcount* counter, size_t* expected, size_t desired) {
  return atomic_compare_exchange_weak_explicit(
      counter, expected, desired, memory_order_relaxed, memory_order_relaxed);
}
static bool ku_atomic_refcount_compare_exchange_acq_rel(
    KuAtomicRefcount* counter, size_t* expected, size_t desired) {
  return atomic_compare_exchange_weak_explicit(
      counter, expected, desired, memory_order_acq_rel, memory_order_relaxed);
}
#endif
static void ku_refcount_retain(KuAtomicRefcount* counter, const char* owner) {
  size_t current = ku_atomic_refcount_load(counter);
  for (;;) {
    if (current == 0) {
      fprintf(stderr, "invalid %s retain\n", owner);
      exit(1);
    }
    if (current >= KU_REFCOUNT_MAX) {
      fprintf(stderr, "%s reference count overflow\n", owner);
      exit(1);
    }
    size_t expected = current;
    if (ku_atomic_refcount_compare_exchange_relaxed(
            counter, &expected, current + 1)) return;
    current = expected;
  }
}
static bool ku_refcount_release(KuAtomicRefcount* counter, const char* owner) {
  size_t current = ku_atomic_refcount_load(counter);
  for (;;) {
    if (current == 0 || current > KU_REFCOUNT_MAX) {
      fprintf(stderr, "invalid %s release\n", owner);
      exit(1);
    }
    size_t expected = current;
    if (ku_atomic_refcount_compare_exchange_acq_rel(
            counter, &expected, current - 1)) return current == 1;
    current = expected;
  }
}

typedef struct KuEnvHeader {
  void (*retain)(void*);
  void (*release)(void*);
  KuAtomicRefcount rc;
} KuEnvHeader;

"#,
    );
}

/// Emit a `typedef struct { ret (*invoke)(void*, params...); void* env; }` for
/// every distinct closure signature the program uses (Stage 6a). Runs in two
/// passes sharing `header_done`/`emitted`: `self_contained_only == true` emits
/// signatures over primitives before the array/result ABI (so array-of-closures
/// resolves `KuClosure_*`); the second pass (`false`) emits the remainder after
/// those ABIs exist (so a closure returning e.g. `[int]` sees `KuArray_int`).
fn emit_closure_types(
    out: &mut COutput,
    program: &IrProgram,
    header_done: &mut bool,
    emitted: &mut std::collections::HashSet<String>,
    self_contained_only: bool,
) -> KuResult<()> {
    out.check()?;
    let mut types = Vec::new();
    collect_closure_types_program(program, &mut types);
    let selected: Vec<&IrType> = types
        .iter()
        .filter(|ty| match ty {
            IrType::Closure { params, ret, .. } => {
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
        emit_closure_refcount_header(out);
        *header_done = true;
    }
    for ty in selected {
        let IrType::Closure {
            params,
            param_modes,
            ret,
        } = ty
        else {
            continue;
        };
        let suffix = closure_signature_suffix(params, param_modes, ret)?;
        if !emitted.insert(suffix.clone()) {
            continue;
        }
        let mut param_list = String::from("void*");
        for (param, mode) in params.iter().zip(param_modes) {
            param_list.push_str(", ");
            param_list.push_str(&c_param_type(param, *mode)?);
        }
        out.push_str(&format!(
            "struct KuClosure_{suffix} {{ {} (*invoke)({}); void* env; }};\n",
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
fn emit_cell_types(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
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
        // The cell owns its payload. Reuse the same exhaustive ownership
        // dispatch as locals/results so structs, enums, Result, KuValue, Error,
        // database handles, and nested closures cannot silently leak here.
        let drop_payload = c_drop_value(inner, "c->value")?;
        out.push_str(&format!(
            "typedef struct {{ {payload} value; KuAtomicRefcount rc; }} KuCell_{suffix};\n\
             static KuCell_{suffix}* ku_cell_{suffix}_new({payload} init) {{ KuCell_{suffix}* c = (KuCell_{suffix}*)malloc(sizeof(KuCell_{suffix})); if (!c) {{ fprintf(stderr, \"out of memory\\n\"); exit(1); }} c->value = init; ku_atomic_refcount_init(&c->rc); return c; }}\n\
             static void ku_cell_{suffix}_retain(KuCell_{suffix}* c) {{ if (c) ku_refcount_retain(&c->rc, \"closure cell\"); }}\n\
             static void ku_cell_{suffix}_release(KuCell_{suffix}* c) {{ if (!c) return; if (ku_refcount_release(&c->rc, \"closure cell\")) {{ {drop_payload}free(c); }} }}\n"
        ));
    }
    out.push('\n');
    Ok(())
}

/// Stage 6b: emit a `KuEnv_{id}` (with type-erased retain/release matching
/// `KuEnvHeader`) for every capturing closure body. The env holds one reference
/// per captured cell (retained on `new`, released on the env's final release).
fn emit_env_types(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut emitted = false;
    for function in &program.functions {
        if !function.is_closure_body || function.captures.is_empty() {
            continue;
        }
        out.check()?;
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
            "typedef struct KuEnv_{id} {{\n  void (*retain)(void*);\n  void (*release)(void*);\n  KuAtomicRefcount rc;\n{fields}}} KuEnv_{id};\n"
        ));
        out.push_str(&format!(
            "static void ku_env_{id}_retain(void* p) {{ KuEnv_{id}* e = (KuEnv_{id}*)p; if (e) ku_refcount_retain(&e->rc, \"closure environment\"); }}\n"
        ));
        out.push_str(&format!(
            "static void ku_env_{id}_release(void* p) {{ KuEnv_{id}* e = (KuEnv_{id}*)p; if (!e) return; if (ku_refcount_release(&e->rc, \"closure environment\")) {{\n{releases}  free(e);\n}} }}\n"
        ));
        out.push_str(&format!(
            "static KuEnv_{id}* ku_env_{id}_new({params}) {{ KuEnv_{id}* e = (KuEnv_{id}*)malloc(sizeof(KuEnv_{id})); if (!e) {{ fprintf(stderr, \"out of memory\\n\"); exit(1); }} e->retain = ku_env_{id}_retain; e->release = ku_env_{id}_release; ku_atomic_refcount_init(&e->rc);\n{assigns}  return e; }}\n"
        ));
        emitted = true;
    }
    if emitted {
        out.push('\n');
    }
    Ok(())
}

/// Function order is not call order: imported functions, mutually recursive
/// functions and monomorphized instances may follow their callers. Use the same
/// signature for declarations and definitions, including borrowed parameters.
fn emit_function_prototypes(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    for function in &program.functions {
        out.push_str(&function_signature(function)?);
        out.push_str(";\n");
    }
    if !program.functions.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn function_signature(function: &IrFunction) -> KuResult<String> {
    let mut params = if function.is_closure_body {
        String::from("void* __env")
    } else {
        String::new()
    };
    for param in &function.params {
        if !params.is_empty() {
            params.push_str(", ");
        }
        params.push_str(&format!(
            "{} {}",
            c_param_type(&param.ty, param.mode)?,
            c_ident(&param.name)
        ));
    }
    if params.is_empty() {
        params.push_str("void");
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
fn emit_closure_thunk_prototypes(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
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
fn emit_closure_thunks(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
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
            out.push_str(&format!(
                "  return {}({});\n",
                c_symbol(&function.name),
                args
            ));
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
        params.push_str(&format!(
            "{} {}",
            c_param_type(&param.ty, param.mode)?,
            c_ident(&param.name)
        ));
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
fn emit_array_map_helpers(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut calls = Vec::new();
    collect_array_map_calls_program(program, &mut calls);
    let mut emitted = std::collections::HashSet::new();
    for (in_element, params, param_modes, ret) in calls {
        let cl_suffix = closure_signature_suffix(&params, &param_modes, &ret)?;
        if !emitted.insert(cl_suffix.clone()) {
            continue;
        }
        let in_array = c_array_type(&in_element)?;
        let out_array = c_array_type(&ret)?;
        let out_type = c_type(&ret)?;
        let out_suffix = c_type_suffix(&ret)?;
        // Each element is cloned before being handed to the mapper (identity for
        // Copy types like int): the input array keeps ownership of its elements
        // while the closure body owns the value it receives.
        let arg = if param_modes.first() == Some(&ParamMode::View) && is_c_owned_type(&in_element) {
            "&array.data[index]".to_string()
        } else {
            c_clone_value(&in_element, "array.data[index]")?
        };
        out.push_str(&format!(
            "static {out_array} ku_array_map_{cl_suffix}({in_array} array, KuClosure_{cl_suffix} mapper) {{\n\
             \x20 {out_array} result = {{ 0, NULL }};\n\
             \x20 int timed_out = 0;\n\
             \x20 if (array.len > 0) {{\n\
             \x20   if (array.len > SIZE_MAX / sizeof({out_type})) {{ fprintf(stderr, \"array allocation is too large\\n\"); exit(1); }}\n\
             \x20   result.data = ({out_type}*)malloc(array.len * sizeof({out_type}));\n\
             \x20   if (!result.data) {{ fprintf(stderr, \"array allocation failed\\n\"); exit(1); }}\n\
             \x20   for (size_t index = 0; index < array.len; index++) {{\n\
             \x20     if (__ku_handler_timeout_poll()) {{ timed_out = 1; break; }}\n\
             \x20     result.data[index] = mapper.invoke(mapper.env, {arg});\n\
             \x20     result.len = index + 1;\n\
             \x20     if (__ku_handler_timeout_poll()) {{ timed_out = 1; break; }}\n\
             \x20   }}\n\
             \x20 }}\n\
             \x20 if (mapper.env) ((KuEnvHeader*)mapper.env)->release(mapper.env);\n\
             \x20 if (timed_out) {{ ku_array_drop_{out_suffix}(&result); return ({out_array}){{0}}; }}\n\
             \x20 return result;\n\
             }}\n\n"
        ));
    }
    Ok(())
}

fn collect_array_map_calls_program(
    program: &IrProgram,
    calls: &mut Vec<(IrType, Vec<IrType>, Vec<ParamMode>, IrType)>,
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

fn collect_array_map_calls_expr(
    expr: &IrExpr,
    calls: &mut Vec<(IrType, Vec<IrType>, Vec<ParamMode>, IrType)>,
) {
    if let IrExprKind::Call {
        kind: IrCallKind::Intrinsic(name),
        args,
        ..
    } = &expr.kind
    {
        if name == "array.map" {
            if let (Some(receiver), Some(mapper)) = (args.first(), args.get(1)) {
                if let (
                    IrType::Array(element),
                    IrType::Closure {
                        params,
                        param_modes,
                        ret,
                    },
                ) = (&receiver.ty, &mapper.ty)
                {
                    calls.push((
                        (**element).clone(),
                        params.clone(),
                        param_modes.clone(),
                        (**ret).clone(),
                    ));
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
        IrType::Closure { params, ret, .. } => {
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
        // A safepoint carries only CFG edges; it has no expression/type payload.
        IrTerminator::Safepoint { .. } => {}
    }
}

fn expr_children(expr: &IrExpr) -> Vec<&IrExpr> {
    match &expr.kind {
        IrExprKind::Borrow(expr) | IrExprKind::Unary { expr, .. } | IrExprKind::TryUnwrap(expr) => {
            vec![expr]
        }
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
        | IrExprKind::BorrowedTemp(_)
        | IrExprKind::BorrowedParam(_)
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
                IrType::Int | IrType::Float | IrType::Bool | IrType::Str => {}
                // Array fields (`[int]`, `[Person]`, `[[int]]`): the array typedef is
                // emitted before the struct layout and the array helpers are forward-
                // declared, so embedding one by value and deep clone/drop both work.
                // The element must itself be an "early" type (primitive/struct/nested
                // array of those); an array of closures/objects is not supported as a
                // struct field.
                IrType::Array(element) if is_early_array_element(element, program) => {}
                IrType::Array(_) => {
                    return Err(unsupported(format!(
                        "native C struct '{}.{}' supports array fields of int/float/bool/str/struct only for now",
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
                    IrType::Int | IrType::Float | IrType::Bool | IrType::Str => {}
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
fn emit_layouts(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
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

fn emit_struct_layout(out: &mut COutput, layout: &IrStructLayout) -> KuResult<()> {
    out.check()?;
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

fn emit_enum_layout(out: &mut COutput, layout: &IrEnumLayout) -> KuResult<()> {
    out.check()?;
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
                    IrInst::CellNew { init, .. } => collect_array_expr_types(init, element_types),
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
    // These runtimes reference `KuArray_str` internally, so force its ABI even
    // when a direct intrinsic call is optimized into a terminator and no array
    // literal/local remains for the structural collector to see.
    if (program_uses_pg(program)
        || program_uses_mysql(program)
        || program_fs_usage(program).read_dir)
        && !element_types.contains(&IrType::Str)
    {
        element_types.push(IrType::Str);
    }
}

/// Forward-declare every user-struct tag so a `KuArray_KuStruct_X` typedef can hold
/// a `KuStruct_X*` before the struct body is emitted. The struct body later completes
/// the same tag (`struct KuStruct_X { ... };`), so `emit_struct_layout` must emit the
/// body form, not another `typedef`.
fn emit_struct_forward_decls(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
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
/// elements are NOT early — their tags are declared here, but their C type bodies
/// are completed later by `emit_late_array_typedefs`.
fn is_early_array_element(element: &IrType, program: &IrProgram) -> bool {
    match element {
        IrType::Int | IrType::Float | IrType::Bool | IrType::Str => true,
        IrType::Array(inner) => is_early_array_element(inner, program),
        IrType::Named(name) => {
            enum_type_name(name).is_none()
                && program
                    .layouts
                    .structs
                    .iter()
                    .any(|layout| layout.name == *name)
        }
        _ => false,
    }
}

/// Emit the `KuArray_E` typedefs whose element is "early" (see `is_early_array_element`)
/// before the struct layouts, plus the shared bounds-fail helper (emitted whenever any
/// array exists at all). Late-element typedefs are emitted by
/// `emit_late_array_typedefs`.
fn emit_array_typedefs(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut element_types = Vec::new();
    collect_all_array_elements(program, &mut element_types);
    if element_types.is_empty() {
        return Ok(());
    }
    out.push_str(
        "static void ku_array_bounds_fail(int64_t index, size_t len) {\n  fprintf(stderr, \"array/index_out_of_bounds: index %lld out of bounds for length %zu\\n\", (long long)index, len);\n  exit(1);\n}\n\n",
    );
    for element in &element_types {
        if !is_early_array_element(element, program) {
            continue;
        }
        let array_type = c_array_type(element)?;
        let element_type = c_type(element)?;
        out.push_str(&format!(
            "typedef struct {{ size_t len; {element_type}* data; size_t capacity; }} {array_type};\n"
        ));
    }
    out.push('\n');
    Ok(())
}

/// Forward-declare the early-element array helpers that the struct ownership pass
/// calls (a struct's deep clone/drop invokes `ku_array_clone_*` / `ku_array_drop_*`
/// for its array fields), so those uses resolve before the helper bodies are emitted.
fn emit_array_helper_prototypes(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut element_types = Vec::new();
    collect_all_array_elements(program, &mut element_types);
    let mut any = false;
    for element in &element_types {
        if !is_early_array_element(element, program) {
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

/// Complete array bodies whose element ABI was only forward-declared, then
/// declare clone/drop for them. Result helpers emitted in the next phase may
/// own any array by value and therefore need these prototypes before the array
/// helper bodies themselves are available.
fn emit_late_array_typedefs(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut element_types = Vec::new();
    collect_all_array_elements(program, &mut element_types);
    let mut any = false;
    for element in &element_types {
        if is_early_array_element(element, program) {
            continue;
        }
        let array_type = c_array_type(element)?;
        let element_type = c_type(element)?;
        out.push_str(&format!(
            "typedef struct {{ size_t len; {element_type}* data; size_t capacity; }} {array_type};\n"
        ));
        any = true;
    }
    for element in &element_types {
        if is_early_array_element(element, program) {
            continue;
        }
        let array_type = c_array_type(element)?;
        let suffix = c_type_suffix(element)?;
        out.push_str(&format!(
            "static {array_type} ku_array_clone_{suffix}({array_type} array);\n\
             static void ku_array_drop_{suffix}({array_type}* array);\n"
        ));
    }
    if any {
        out.push('\n');
    }
    Ok(())
}

/// Emit all array ownership/access helper bodies after Result and closure types
/// are complete. Collection order is inner-before-outer, so nested arrays also
/// see the helper definitions they call.
fn emit_array_helper_bodies(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut element_types = Vec::new();
    collect_all_array_elements(program, &mut element_types);
    for element in &element_types {
        emit_array_helpers_for(out, element)?;
    }
    Ok(())
}

/// Emit the `KuArray_<suffix>` helper bodies (make/clone/move/drop/get/at/len/
/// is_empty/push/push_reuse) for one element type. The typedef is emitted separately by
/// `emit_array_typedefs`.
fn emit_array_helpers_for(out: &mut COutput, element: &IrType) -> KuResult<()> {
    out.check()?;
    let array_type = c_array_type(element)?;
    let suffix = c_type_suffix(element)?;
    let element_type = c_type(element)?;
    let clone_element = c_clone_value(element, "array.data[index]")?;
    let drop_element = c_drop_value(element, "array->data[index]")?;
    let clone_pushed = c_clone_value(element, "value")?;
    out.push_str(&format!(
        "static {array_type} ku_array_make_{suffix}(size_t len, const {element_type}* values) {{\n\
             \x20 {array_type} result = {{ len, NULL, len }};\n\
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
             \x20 *array = ({array_type}){{0}};\n\
             \x20 return result;\n\
             }}\n\
             static void ku_array_drop_{suffix}({array_type}* array) {{\n\
             \x20 if (!array) return;\n\
             \x20 if (array->data) {{\n\
             \x20   for (size_t index = 0; index < array->len; index++) {{ {drop_element} }}\n\
             \x20   free(array->data);\n\
             \x20 }}\n\
             \x20 *array = ({array_type}){{0}};\n\
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
             \x20 if (array.len == SIZE_MAX) {{ fprintf(stderr, \"array allocation is too large\\n\"); exit(1); }}\n\
             \x20 size_t len = array.len + 1;\n\
             \x20 if (len > SIZE_MAX / sizeof({element_type})) {{ fprintf(stderr, \"array allocation is too large\\n\"); exit(1); }}\n\
             \x20 {element_type}* data = ({element_type}*)malloc(len * sizeof({element_type}));\n\
             \x20 if (!data) {{ fprintf(stderr, \"array allocation failed\\n\"); exit(1); }}\n\
             \x20 for (size_t index = 0; index < array.len; index++) data[index] = {clone_element};\n\
             \x20 data[array.len] = {clone_pushed};\n\
             \x20 return ({array_type}){{ len, data, len }};\n\
             }}\n\
             /* Internal same-local assignment only; ordinary push stays pure.\n\
                Legacy two-field initializers have capacity zero and own len slots. */\n\
             static {array_type} ku_array_push_reuse_{suffix}({array_type}* array, {element_type} value) {{\n\
             \x20 size_t len = ku_size_add(array->len, 1, \"array allocation\");\n\
             \x20 size_t capacity = array->capacity > array->len ? array->capacity : array->len;\n\
             \x20 size_t grown = ku_collection_capacity(capacity, len, sizeof({element_type}), \"array allocation\");\n\
             \x20 {element_type} pushed = {clone_pushed};\n\
             \x20 {array_type} result = *array;\n\
             \x20 if (!result.data || len > capacity) {{\n\
             \x20   {element_type}* data = ({element_type}*)realloc(result.data, grown * sizeof({element_type}));\n\
             \x20   if (!data) {{ fprintf(stderr, \"array allocation failed\\n\"); exit(1); }}\n\
             \x20   result.data = data;\n\
             \x20   capacity = grown;\n\
             \x20 }}\n\
             \x20 result.data[result.len] = pushed;\n\
             \x20 result.len = len;\n\
             \x20 result.capacity = capacity;\n\
             \x20 *array = ({array_type}){{0}};\n\
             \x20 return result;\n\
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
        IrExprKind::Borrow(expr) | IrExprKind::Unary { expr, .. } | IrExprKind::TryUnwrap(expr) => {
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
        | IrExprKind::BorrowedTemp(_)
        | IrExprKind::BorrowedParam(_)
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

/// Reject malformed IR before formatting any C `goto` statements. Leaving an
/// unknown target for the C compiler would turn an internal CFG error into a
/// late, toolchain-specific missing-label diagnostic. Keep this edge set in
/// lockstep with every explicit-target terminator, including both Safepoint
/// successors.
fn validate_cfg(function: &IrFunction) -> KuResult<()> {
    let mut block_ids = HashSet::new();
    for block in &function.blocks {
        if !block_ids.insert(block.id) {
            return Err(unsupported(format!(
                "native C function '{}' has duplicate block id {}",
                function.name, block.id.0
            )));
        }
    }

    for block in &function.blocks {
        let mut targets = Vec::new();
        match &block.terminator {
            IrTerminator::Jump(target) => targets.push(*target),
            IrTerminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                targets.push(*then_block);
                targets.push(*else_block);
            }
            IrTerminator::ForEach {
                body_block,
                after_block,
                ..
            } => {
                targets.push(*body_block);
                targets.push(*after_block);
            }
            IrTerminator::ResultBranch {
                ok_block,
                err_block,
                ..
            } => {
                targets.push(*ok_block);
                targets.push(*err_block);
            }
            IrTerminator::Safepoint {
                continue_block,
                timeout_block,
            } => {
                targets.push(*continue_block);
                targets.push(*timeout_block);
            }
            IrTerminator::JumpErr { target, .. } => targets.push(*target),
            IrTerminator::Next
            | IrTerminator::PropagateErr(_)
            | IrTerminator::Return(_)
            | IrTerminator::Unreachable => {}
        }

        for target in targets {
            if !block_ids.contains(&target) {
                return Err(unsupported(format!(
                    "native C function '{}' block {} branches to missing block {}",
                    function.name, block.id.0, target.0
                )));
            }
        }
    }
    Ok(())
}

fn emit_function(out: &mut COutput, function: &IrFunction) -> KuResult<()> {
    out.check()?;
    let for_each_states = collect_for_each_states(function)?;
    let owned_locals = collect_owned_locals(function, &for_each_states);
    out.push_str(&function_signature(function)?);
    out.push_str(" {\n");
    out.push_str("  if (++__ku_call_depth > KU_MAX_CALL_DEPTH) { fprintf(stderr, \"maximum call depth exceeded: %d\\n\", KU_MAX_CALL_DEPTH); exit(1); }\n");
    // Set only when this frame itself takes a Safepoint timeout edge. While the
    // frame runs its finally route, TLS unwind_depth grants one bounded cleanup
    // window to this frame and helpers called by it. After that shared grace
    // expires, the same frame may take another timeout edge, so the local flag
    // also keeps its unwind-depth contribution idempotent and balanced.
    out.push_str("  int __ku_timeout_unwind = 0;\n");
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
    for state in &for_each_states {
        let prefix = for_state_prefix(state.block_id);
        let index_type = if state.iterable_ty == IrType::Int {
            "uint64_t"
        } else {
            "size_t"
        };
        out.push_str(&format!(
            "  bool {prefix}_initialized = false;\n  {index_type} {prefix}_index = 0;\n"
        ));
        if state.iterable_ty == IrType::Int {
            out.push_str(&format!("  int64_t {prefix}_limit = 0;\n"));
        }
        if !is_c_owned_type(&state.element_ty) {
            out.push_str(&format!(
                "  {} {} = {};\n",
                c_type(&state.element_ty)?,
                c_ident(&state.name),
                c_zero_initializer(&state.element_ty)?
            ));
        }
    }
    for local in &owned_locals {
        out.check()?;
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
        emit_block(
            out,
            block,
            &function.return_type,
            &owned_locals,
            &for_each_states,
        )?;
    }
    if function.return_type == IrType::Void {
        emit_owned_cleanup(out, &owned_locals)?;
        out.push_str("  if (__ku_timeout_unwind) __ku_handler_timeout_leave();\n");
        out.push_str("  __ku_call_depth--;\n");
        out.push_str("  return;\n");
    }
    out.push_str("}\n");
    Ok(())
}

fn emit_block(
    out: &mut COutput,
    block: &IrBlock,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
    for_each_states: &[ForEachState],
) -> KuResult<()> {
    out.check()?;
    if block.id.0 != 0 {
        out.push_str(&format!("block{}:;\n", block.id.0));
    }
    for state in for_each_states
        .iter()
        .filter(|state| state.after_block == block.id)
    {
        emit_for_each_cleanup(out, state)?;
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
    emit_terminator(out, block.id, &block.terminator, return_type, owned_locals)
}

fn emit_inst(
    out: &mut COutput,
    inst: &IrInst,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    out.check()?;
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
                let borrowed = crate::ir::ir_expr_is_borrowed(value)
                    || matches!(
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
                // (e.g. `result.rows()` after `client.query(..)?`), the temp is reused across a loop
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
                out.push_str(&format!(
                    "  {{ {} __ku_store = {};\n",
                    c_type(ty)?,
                    materialized
                ));
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
            if dynamic_object_store_target(target) {
                emit_dynamic_object_store(out, target, value)?;
                return Ok(());
            }
            // Every non-local owned place (a struct field or array slot) owns its
            // current payload. Materialize the RHS first, resolve the destination
            // address once, drop the old payload, then install the new owner.
            // Caching the slot pointer also prevents an indexed projection from
            // repeating bounds checks or any nested place calculation.
            if matches!(target, IrLValue::Field { .. } | IrLValue::Index { .. })
                && is_c_owned_type(&value.ty)
            {
                let lvalue = c_lvalue(target)?;
                out.push_str(&format!(
                    "  {{ {} __ku_store = {};\n  {}* __ku_slot = &({lvalue});\n",
                    c_type(&value.ty)?,
                    c_value_expr(value)?,
                    c_type(&value.ty)?,
                ));
                emit_drop_expr(out, &value.ty, "(*__ku_slot)")?;
                out.push_str("  *__ku_slot = __ku_store; }\n");
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
                c_error_expr(value, "fail")?
            );
            // A fail expression can move owned message/domain/code temporaries.
            // Materialize that owner before frame cleanup, just like return and
            // error propagation, or cleanup clears/frees its source strings.
            out.push_str(&format!(
                "  {{ {} __ku_return = {result};\n",
                c_type(return_type)?
            ));
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str("  if (__ku_timeout_unwind) __ku_handler_timeout_leave();\n");
            out.push_str("  __ku_call_depth--;\n");
            out.push_str("  return __ku_return; }\n");
        }
        IrInst::Panic(value) => {
            if value.ty == IrType::Str {
                out.push_str(&format!(
                    "  {{ KuString __ku_panic = {}; ku_string_write(stderr, __ku_panic); fputc('\\n', stderr); exit(1); }}\n",
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
                Some(inner) if is_c_owned_type(&inner) => {
                    let payload = c_type(&inner)?;
                    let suffix = c_type_suffix(&inner)?;
                    let drop_old = c_drop_value(&inner, "__ku_cell_ptr->value")?;
                    out.push_str(&format!(
                        "  {{ {payload} __ku_cell_new = {}; KuCell_{suffix}* __ku_cell_ptr = {}; {drop_old} __ku_cell_ptr->value = __ku_cell_new; }}\n",
                        c_value_expr(value)?,
                        c_expr(cell)?,
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

fn emit_expr_statement(out: &mut COutput, value: &IrExpr) -> KuResult<()> {
    out.check()?;
    if let IrExprKind::Call {
        kind: IrCallKind::Intrinsic(name),
        args,
        ..
    } = &value.kind
    {
        if name == "__ku_drop_borrow_temp" {
            let [owner] = args.as_slice() else {
                return Err(unsupported("borrow temporary cleanup expects one owner"));
            };
            return emit_drop_expr(out, &owner.ty, &c_addressable_expr(owner)?);
        }
    }
    if emit_statement_intrinsic(out, value)? {
        return Ok(());
    }
    out.push_str(&format!("  (void){};\n", c_expr(value)?));
    Ok(())
}

fn emit_statement_intrinsic(out: &mut COutput, value: &IrExpr) -> KuResult<bool> {
    out.check()?;
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

fn emit_print(out: &mut COutput, value: &IrExpr) -> KuResult<()> {
    out.check()?;
    match value.ty {
        IrType::Int => {
            out.push_str(&format!(
                "  printf(\"%lld\", (long long){});\n  fflush(stdout);\n",
                c_expr(value)?
            ));
        }
        IrType::Float => {
            out.push_str(&format!(
                "  printf(\"%.17g\", (double){});\n  fflush(stdout);\n",
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
                "  {{ KuString __ku_p = {}; ku_string_write(stdout, __ku_p); }}\n  fflush(stdout);\n",
                c_expr(value)?
            ));
        }
        IrType::Named(ref name) if name == "__ku_value" => {
            out.push_str(&format!(
                "  ku_value_print({});\n  fflush(stdout);\n",
                c_expr(value)?
            ));
        }
        IrType::Named(ref name) if name == "__ku_time" => {
            out.push_str(&format!(
                "  ku_time_print({});\n  fflush(stdout);\n",
                c_expr(value)?
            ));
        }
        _ => {
            return Err(unsupported(
                "native C prototype print supports int/float/bool/str/KuValue",
            ))
        }
    }
    Ok(())
}

fn emit_for_each_cleanup(out: &mut COutput, state: &ForEachState) -> KuResult<()> {
    out.check()?;
    let prefix = for_state_prefix(state.block_id);
    out.push_str(&format!("  if ({prefix}_initialized) {{\n"));
    if is_c_owned_type(&state.element_ty) {
        emit_drop_expr(out, &state.element_ty, &c_ident(&state.name))?;
    }
    if let IrType::Array(element) = &state.iterable_ty {
        emit_drop_expr(
            out,
            &IrType::Array(element.clone()),
            &format!("{prefix}_array"),
        )?;
    }
    out.push_str(&format!(
        "  {prefix}_initialized = false;\n  {prefix}_index = 0;\n  }}\n"
    ));
    Ok(())
}

fn emit_terminator(
    out: &mut COutput,
    block_id: crate::ir::BlockId,
    terminator: &IrTerminator,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    out.check()?;
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
        IrTerminator::ForEach {
            name,
            iterable,
            body_block,
            after_block,
        } => {
            let prefix = for_state_prefix(block_id);
            match &iterable.ty {
                IrType::Int => {
                    out.push_str(&format!(
                        "  if (!{prefix}_initialized) {{\n  {prefix}_limit = {};\n  if ({prefix}_limit < 0) {{ fprintf(stderr, \"for int iterator expects a non-negative int\\n\"); exit(1); }}\n  {prefix}_index = 0;\n  {prefix}_initialized = true;\n  }}\n",
                        c_expr(iterable)?
                    ));
                    out.push_str(&format!(
                        "  if ({prefix}_index < (uint64_t){prefix}_limit) {{ {} = (int64_t){prefix}_index++; goto block{}; }} else goto block{};\n",
                        c_ident(name), body_block.0, after_block.0
                    ));
                }
                IrType::Array(element) => {
                    let snapshot = format!("{prefix}_array");
                    out.push_str(&format!(
                        "  if (!{prefix}_initialized) {{\n  {snapshot} = {};\n  {prefix}_index = 0;\n  {prefix}_initialized = true;\n  }}\n",
                        c_clone_expr(iterable)?
                    ));
                    let slot = format!("{snapshot}.data[{prefix}_index]");
                    if is_c_owned_type(element) {
                        out.push_str(&format!(
                            "  if ({prefix}_index < {snapshot}.len) {{ {} __ku_for_value = {};\n",
                            c_type(element)?,
                            c_move_value(element, &slot)?
                        ));
                        emit_drop_expr(out, element, &c_ident(name))?;
                        out.push_str(&format!(
                            "  {} = __ku_for_value; {prefix}_index++; goto block{}; }} else goto block{};\n",
                            c_ident(name), body_block.0, after_block.0
                        ));
                    } else {
                        out.push_str(&format!(
                            "  if ({prefix}_index < {snapshot}.len) {{ {} = {slot}; {prefix}_index++; goto block{}; }} else goto block{};\n",
                            c_ident(name), body_block.0, after_block.0
                        ));
                    }
                }
                other => {
                    return Err(unsupported(format!(
                        "native C for expects array or int but got {other}"
                    )));
                }
            }
            Ok(())
        }
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
            out.push_str("  if (__ku_timeout_unwind) __ku_handler_timeout_leave();\n");
            out.push_str("  __ku_call_depth--;\n");
            out.push_str(&format!(
                "  return ({}){{ false, {}, __ku_error }}; }}\n",
                c_type(return_type)?,
                c_zero_value(return_inner)?
            ));
            Ok(())
        }
        IrTerminator::Safepoint {
            continue_block,
            timeout_block,
        } => {
            // The poll has no cleanup or socket side effects. Its timeout edge was
            // built by the IR lowerer's `return_terminator`, so it executes every
            // active finally block before the ordinary return emitter performs the
            // frame's owned-local cleanup.
            out.push_str(&format!(
                "  if (__ku_handler_timeout_poll()) {{ if (!__ku_timeout_unwind) {{ __ku_handler_timeout_enter(); __ku_timeout_unwind = 1; }} goto block{}; }} else goto block{};\n",
                timeout_block.0, continue_block.0
            ));
            Ok(())
        }
        IrTerminator::Return(Some(value)) => {
            // A Copy payload can still live inside an owned cell. Read it
            // before cleanup releases that cell, just as owned returns must be
            // moved out before their source owner is dropped.
            if is_c_owned_type(&value.ty) || matches!(value.kind, IrExprKind::CellLoad(_)) {
                out.push_str(&format!(
                    "  {{ {} __ku_return = {};\n",
                    c_type(&value.ty)?,
                    c_value_expr(value)?
                ));
                emit_owned_cleanup(out, owned_locals)?;
                out.push_str("  if (__ku_timeout_unwind) __ku_handler_timeout_leave();\n");
                out.push_str("  __ku_call_depth--;\n");
                out.push_str("  return __ku_return; }\n");
            } else {
                let value = c_value_expr(value)?;
                emit_owned_cleanup(out, owned_locals)?;
                out.push_str("  if (__ku_timeout_unwind) __ku_handler_timeout_leave();\n");
                out.push_str("  __ku_call_depth--;\n");
                out.push_str(&format!("  return {value};\n"));
            }
            Ok(())
        }
        IrTerminator::Return(None) => {
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str("  if (__ku_timeout_unwind) __ku_handler_timeout_leave();\n");
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
        IrExprKind::BorrowedParam(name) => Ok(if is_c_owned_type(&expr.ty) {
            format!("(*{})", c_symbol(name))
        } else {
            c_symbol(name)
        }),
        IrExprKind::Borrow(value) => c_expr(value),
        IrExprKind::Temp(id) | IrExprKind::BorrowedTemp(id) => Ok(format!("t{}", id.0)),
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
            if matches!(&left.ty, IrType::Named(name) if name == "__ku_time")
                && matches!(&right.ty, IrType::Named(name) if name == "__ku_time")
                && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            Ok(format!(
                "(ku_time_equal({}, {}) {} true)",
                c_expr(left)?,
                c_expr(right)?,
                if *op == BinaryOp::Equal { "==" } else { "!=" }
            ))
        }
        IrExprKind::Binary { left, op, right }
            if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual)
                && (ir_type_is_ku_value(&left.ty) || ir_type_is_ku_value(&right.ty)) =>
        {
            let equality = if ir_type_is_ku_value(&left.ty) && ir_type_is_ku_value(&right.ty) {
                format!("ku_value_equal({}, {})", c_expr(left)?, c_expr(right)?)
            } else {
                let (dynamic, typed) = if ir_type_is_ku_value(&left.ty) {
                    (left.as_ref(), right.as_ref())
                } else {
                    (right.as_ref(), left.as_ref())
                };
                if let IrType::Array(element) = &typed.ty {
                    format!(
                        "ku_value_equal_typed_array_{}({}, {})",
                        c_type_suffix(element)?,
                        c_expr(dynamic)?,
                        c_expr(typed)?
                    )
                } else if let Some(wrapped) = ku_value_borrow_wrap(&typed.ty, &c_expr(typed)?)? {
                    format!("ku_value_equal({}, {wrapped})", c_expr(dynamic)?)
                } else {
                    // KuValue has no tag for this native type. IR lowering has
                    // already materialized effectful operands, but retain both
                    // C evaluations here so borrowed locals/temps remain visible
                    // to sanitizers and future lowering changes.
                    format!(
                        "((void)({}), (void)({}), false)",
                        c_expr(left)?,
                        c_expr(right)?
                    )
                }
            };
            if *op == BinaryOp::NotEqual {
                Ok(format!("(!({equality}))"))
            } else {
                Ok(format!("({equality})"))
            }
        }
        IrExprKind::Binary { left, op, right }
            if left.ty == IrType::Str && right.ty == IrType::Str && *op == BinaryOp::Add =>
        {
            Ok(format!(
                "ku_string_concat({}, {})",
                c_expr(left)?,
                c_expr(right)?
            ))
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
                if name == "__ku_clone" {
                    let value = args
                        .first()
                        .ok_or_else(|| unsupported("clone intrinsic requires one argument"))?;
                    return c_clone_expr(value);
                }
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
        IrExprKind::Field { target, name } if matches!(&target.ty, IrType::Named(type_name) if type_name == "__ku_time") => {
            match name.as_str() {
                "kind" => Ok(format!("ku_time_kind({})", c_expr(target)?)),
                "millis" => Ok(format!("ku_time_value_millis({})", c_expr(target)?)),
                _ => Err(unsupported(format!(
                    "native Time value has no field '{name}'"
                ))),
            }
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
                    .map(|(name, _, source)| match source {
                        IrCaptureSource::Local => c_symbol(name),
                        IrCaptureSource::EnclosingEnvironment => {
                            format!("__e->{}", c_ident(name))
                        }
                    })
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
                || type_name == "__ku_time"
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
    if let IrExprKind::Call { kind, args, .. } = &expr.kind {
        if matches!(kind, IrCallKind::Intrinsic(name) if name == "__ku_clone") {
            let value = args
                .first()
                .ok_or_else(|| unsupported("clone intrinsic requires one argument"))?;
            return c_clone_expr(value);
        }
    }
    // Stage 8a: the server value is a shared heap pointer and Time is a Copy
    // value; ordinary assignment/passing copies either one. Keep this after the
    // explicit clone intrinsic so `time.instant().clone()` is lowered as a copy
    // instead of falling through to the generic intrinsic dispatcher.
    if matches!(&expr.ty, IrType::Named(name) if name == "__ku_http_server" || name == "__ku_time")
    {
        return c_expr(expr);
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
            if let IrExprKind::Field {
                target,
                name: field,
            } = &expr.kind
            {
                return Ok(format!("ku_error_move(&({}).{})", c_expr(target)?, field));
            }
        }
        if let Some(place) = c_move_place(expr)? {
            return Ok(if name == "__ku_object" {
                format!("ku_object_move(&{place})")
            } else if name == "__ku_value" {
                format!("ku_value_move(&{place})")
            } else if name == "__ku_error_type" {
                format!("ku_error_move(&{place})")
            } else {
                format!("{}(&{})", c_named_move_function(name), place)
            });
        }
    }
    c_expr(expr)
}

/// Borrowed arguments pass a const pointer (Copy scalars remain by value).
/// Every Owned slot, including a function value, uses the usual move semantics.
fn c_borrow_is_addressable(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::Local(_)
        | IrExprKind::Temp(_)
        | IrExprKind::BorrowedTemp(_)
        | IrExprKind::BorrowedParam(_)
        | IrExprKind::CellLoad(_) => true,
        IrExprKind::Field { target, .. } => c_borrow_is_addressable(target),
        _ => false,
    }
}

fn c_arg_value_expr(expr: &IrExpr) -> KuResult<String> {
    if let IrExprKind::Borrow(value) = &expr.kind {
        if !is_c_owned_type(&value.ty) {
            return c_expr(value);
        }
        return match &value.kind {
            IrExprKind::BorrowedParam(name) => Ok(c_symbol(name)),
            _ if c_borrow_is_addressable(value) => Ok(format!("&({})", c_expr(value)?)),
            // A lookup returns a shallow header. Its C11 compound-literal slot
            // remains valid throughout this synchronous call, without ownership.
            _ => Ok(format!(
                "&(({}[]){{ {} }})[0]",
                c_type(&value.ty)?,
                c_expr(value)?
            )),
        };
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
        IrType::Named(name) if name == "__ku_error_type" => {
            Ok(format!("ku_error_clone({})", c_expr(expr)?))
        }
        IrType::Named(name) if name == "__ku_object" => {
            Ok(format!("ku_object_clone({})", c_expr(expr)?))
        }
        IrType::Named(name) if name == "__ku_value" => {
            Ok(format!("ku_value_clone({})", c_expr(expr)?))
        }
        IrType::Named(name) if name == "__ku_time" => c_expr(expr),
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

fn c_error_expr(value: &IrExpr, default_code: &str) -> KuResult<String> {
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
        Ok(format!(
            "ku_error_make({}, {}, {})",
            c_static_utf8_string("ku"),
            c_static_utf8_string(default_code),
            c_value_expr(value)?
        ))
    } else {
        Err(unsupported(
            "native C errors currently require a string or Error value",
        ))
    }
}

/// Whether this Store writes a dynamic object through either its string index or
/// a field-name projection. These two lvalues are not physical C lvalues: object
/// entries live in an open-addressed table and their values are tagged.
fn dynamic_object_store_target(target: &IrLValue) -> bool {
    match target {
        IrLValue::Field { target, .. } | IrLValue::Index { target, .. } => {
            matches!(&target.ty, IrType::Named(name) if name == "__ku_object")
        }
        IrLValue::Local(_) => false,
    }
}

/// Lower `obj[key] = value` / `obj.field = value` without exposing the object
/// hash-table representation as a C lvalue. The IR has already evaluated the
/// RHS into a temp before this Store; materializing its KuValue before evaluating
/// the receiver/key keeps the interpreter's RHS-before-target order. The table
/// helper borrows the key and only clones it if inserting a new entry.
fn emit_dynamic_object_store(out: &mut COutput, target: &IrLValue, value: &IrExpr) -> KuResult<()> {
    out.check()?;
    let (object, key) = match target {
        IrLValue::Index { target, index } => (c_expr(target)?, c_expr(index)?),
        IrLValue::Field { target, name } => (c_expr(target)?, c_static_utf8_string(name)),
        IrLValue::Local(_) => unreachable!("dynamic object Store must be a projection"),
    };

    out.push_str("  {\n    KuValue __ku_object_store_value = ku_v_null();\n");
    if let IrType::Array(element) = &value.ty {
        out.push_str(&format!(
            "    if (!ku_try_v_typed_array_{}({}, &__ku_object_store_value)) {{\n      ku_object_hard_fail_oom();\n    }}\n",
            c_type_suffix(element)?,
            c_value_expr(value)?
        ));
    } else {
        out.push_str(&format!(
            "    __ku_object_store_value = {};\n",
            ku_value_wrap(&value.ty, &c_value_expr(value)?)?
        ));
    }
    out.push_str(&format!(
        "    KuObject* __ku_object_store_target = {object};\n    KuString __ku_object_store_key = {key};\n    if (!ku_object_try_set_copy_key(__ku_object_store_target, __ku_object_store_key, &__ku_object_store_value)) {{\n      ku_value_drop(&__ku_object_store_value);\n      ku_object_hard_fail_oom();\n    }}\n  }}\n"
    ));
    Ok(())
}

fn c_lvalue(target: &IrLValue) -> KuResult<String> {
    match target {
        IrLValue::Local(name) => Ok(c_ident(name)),
        IrLValue::Field { target, name } => {
            if matches!(&target.ty, IrType::Named(type_name) if type_name == "__ku_time") {
                return match name.as_str() {
                    // `millis` is the one public Time field backed by physical
                    // storage in KuTime. `kind` is a virtual discriminator
                    // returned by ku_time_kind(), so treating it as a C lvalue
                    // would emit `(time).kind` even though no such member exists.
                    "millis" => Ok(format!("({}).millis", c_place_expr(target)?)),
                    "kind" => Err(unsupported("native Time.kind is read-only")),
                    _ => Err(unsupported(format!(
                        "native Time value has no field '{name}'"
                    ))),
                };
            }
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
            Ok(format!(
                "({}){}{}",
                c_place_expr(target)?,
                op,
                c_ident(name)
            ))
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
                c_place_expr(target)?,
                c_expr(index)?
            ))
        }
    }
}

/// Emit an addressable C place for an IR projection. Unlike `c_expr`, an Index
/// descends through `ku_array_at_*` and therefore refers to the actual slot rather
/// than a `ku_array_get_*` value copy. This is what makes `values[i].field = rhs`
/// and nested indexed assignments update their owning container.
fn c_place_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.kind {
        IrExprKind::Local(name) => Ok(c_symbol(name)),
        IrExprKind::Temp(id) => Ok(format!("t{}", id.0)),
        IrExprKind::CellLoad(cell) => Ok(format!("({})->value", c_expr(cell)?)),
        IrExprKind::Field { target, name } => {
            if matches!(&target.ty, IrType::Named(type_name) if type_name == "__ku_time") {
                return match name.as_str() {
                    "millis" => Ok(format!("({}).millis", c_place_expr(target)?)),
                    "kind" => Err(unsupported("native Time.kind is read-only")),
                    _ => Err(unsupported(format!(
                        "native Time value has no field '{name}'"
                    ))),
                };
            }
            let op = match c_type(&target.ty) {
                Ok(ty) if ty.ends_with('*') => "->",
                _ => ".",
            };
            Ok(format!(
                "({}){}{}",
                c_place_expr(target)?,
                op,
                c_ident(name)
            ))
        }
        IrExprKind::Index { target, index } => {
            let IrType::Array(element) = &target.ty else {
                return Err(unsupported(
                    "native C indexed place requires an array target",
                ));
            };
            Ok(format!(
                "*ku_array_at_{}(&({}), {})",
                c_type_suffix(element)?,
                c_place_expr(target)?,
                c_expr(index)?
            ))
        }
        _ => Err(unsupported(
            "native C assignment target is not an addressable place",
        )),
    }
}

fn c_type(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int => Ok("int64_t".to_string()),
        IrType::Float => Ok("double".to_string()),
        IrType::Bool => Ok("bool".to_string()),
        IrType::Str => Ok("KuString".to_string()),
        IrType::Null => Ok("uint8_t".to_string()),
        IrType::Array(inner) => c_array_type(inner),
        IrType::Result(inner) => c_result_type(inner),
        IrType::Named(name) if name == "__ku_error_type" => Ok("KuError".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok("KuObject*".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("KuValue".to_string()),
        IrType::Named(name) if name == "__ku_time" => Ok("KuTime".to_string()),
        // Stage 8a: the native HTTP server is a heap pointer shared between route
        // registration and the accept loop (never copied by value).
        IrType::Named(name) if name == "__ku_http_server" => Ok("KuHttpServer*".to_string()),
        // pg opaque handles are the raw libpq pointers; NULL means moved-out/closed.
        IrType::Named(name) if name == "__ku_pg_result" => Ok("KuPgResult*".to_string()),
        IrType::Named(name) if name == "__ku_pg_client" => Ok("KuPgClient*".to_string()),
        IrType::Named(name) if name == "__ku_redis_client" => Ok("KuRedisClient*".to_string()),
        IrType::Named(name) if name == "__ku_bytes" => Ok("KuBytes".to_string()),
        IrType::Named(name) if name == "__ku_net_client" => Ok("KuNetClient*".to_string()),
        IrType::Named(name) if name == "__ku_mysql_client" => Ok("KuMysqlClient*".to_string()),
        IrType::Named(name) if name == "__ku_mysql_result" => Ok("KuMysqlResult*".to_string()),
        IrType::Named(name) => Ok(match enum_type_name(name) {
            Some(name) => c_enum_type(name),
            None => c_struct_type(name),
        }),
        IrType::Closure {
            params,
            param_modes,
            ret,
        } => Ok(format!(
            "KuClosure_{}",
            closure_signature_suffix(params, param_modes, ret)?
        )),
        IrType::Cell(inner) => Ok(format!("KuCell_{}*", c_type_suffix(inner)?)),
        IrType::Void => Ok("void".to_string()),
        _ => Err(unsupported(format!(
            "native C prototype does not support type {ty}"
        ))),
    }
}

fn emit_result_forward_decls(
    out: &mut COutput,
    program: &IrProgram,
    extra_types: &[IrType],
) -> KuResult<()> {
    out.check()?;
    emit_result_abi_phase(out, program, true, extra_types)
}

fn emit_result_abi(out: &mut COutput, program: &IrProgram, extra_types: &[IrType]) -> KuResult<()> {
    out.check()?;
    emit_result_abi_phase(out, program, false, extra_types)
}

/// Collect Result types once per phase through the same path so the early tag
/// declarations cannot drift from runtime-forced Result ABIs (fs/http/db).
fn emit_result_abi_phase(
    out: &mut COutput,
    program: &IrProgram,
    forward_decls_only: bool,
    extra_types: &[IrType],
) -> KuResult<()> {
    out.check()?;
    let mut result_types = extra_types.to_vec();
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
                // The timeout and continuation targets do not introduce a Result
                // ABI; any timeout return payload lives in its target block.
                IrTerminator::Safepoint { .. } => {}
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
        forced.push(IrType::Named("__ku_pg_result".to_string()));
        // The PG runtime is emitted as one unit and always defines the strict cell
        // accessors, even when the source only calls the legacy `value` helper.
        forced.push(IrType::Str);
        forced.push(IrType::Bool);
    }
    if program_uses_pg_client(program) {
        forced.push(IrType::Named("__ku_pg_client".to_string()));
    }
    if program_uses_redis(program) {
        forced.push(IrType::Named("__ku_redis_client".to_string()));
        forced.push(IrType::Null);
        forced.push(IrType::Str);
        forced.push(IrType::Int);
        forced.push(IrType::Bool);
    }
    if program_uses_bytes(program) {
        forced.push(IrType::Named("__ku_bytes".to_string()));
        forced.push(IrType::Str);
        forced.push(IrType::Int);
    }
    if program_uses_net(program) {
        forced.push(IrType::Named("__ku_net_client".to_string()));
        forced.push(IrType::Named("__ku_bytes".to_string()));
        forced.push(IrType::Null);
    }
    if program_uses_mysql(program) {
        forced.push(IrType::Named("__ku_mysql_client".to_string()));
        forced.push(IrType::Named("__ku_mysql_result".to_string()));
        forced.push(IrType::Int);
        forced.push(IrType::Str);
        forced.push(IrType::Bool);
    }
    let fs_usage = program_fs_usage(program);
    if fs_usage.read || fs_usage.try_read {
        forced.push(IrType::Str);
    }
    if fs_usage.write || fs_usage.try_write {
        forced.push(IrType::Null);
    }
    if fs_usage.read_dir {
        forced.push(IrType::Array(Box::new(IrType::Str)));
    }
    for t in forced {
        if !result_types.contains(&t) {
            result_types.push(t);
        }
    }
    // The HTTP runtime is emitted as one unit and always defines both the
    // response adapter and `ku_http_listen[_err]`, so their Result ABIs must
    // exist even when the source never calls listen and no handler returns a
    // Result.
    if program_uses_http(program) {
        for inner in [
            IrType::Named("__ku_http_response".to_string()),
            IrType::Null,
        ] {
            if !result_types.contains(&inner) {
                result_types.push(inner);
            }
        }
    }
    for inner in &result_types {
        let result_type = c_result_type(inner)?;
        if forward_decls_only {
            out.push_str(&format!("typedef struct {result_type} {result_type};\n"));
            continue;
        }
        let suffix = c_type_suffix(inner)?;
        let value_type = c_type(inner)?;
        out.push_str(&format!(
            "struct {result_type} {{ bool ok; {value_type} value; KuError error; }};\n"
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
fn emit_object_abi(
    out: &mut COutput,
    program: &IrProgram,
    object_oom_fault_injection: bool,
) -> KuResult<()> {
    out.check()?;
    if !program_uses_object(program) {
        return Ok(());
    }
    out.push_str(if object_oom_fault_injection {
        "#define KU_NATIVE_TEST_OBJECT_OOM 1\n"
    } else {
        "#define KU_NATIVE_TEST_OBJECT_OOM 0\n"
    });
    out.push_str(
        r#"typedef enum { KU_NULL=0, KU_INT, KU_FLOAT, KU_BOOL, KU_STR, KU_OBJECT, KU_ARRAY, KU_FUNCTION } KuValueTag;
typedef struct KuValue KuValue;
typedef struct KuObject KuObject;
typedef struct KuValueArray KuValueArray;
struct KuValue { KuValueTag tag; union { int64_t i; double f; bool b; KuString s; KuObject* o; KuValueArray* arr; struct { void* invoke; void* env; } fn; } as; };
typedef struct { KuString key; KuValue value; bool used; } KuEntry;
struct KuObject { size_t len; size_t cap; KuEntry* entries; };
struct KuValueArray { size_t len; size_t cap; KuValue* data; };

static KuValue ku_v_null(void) { KuValue v; v.tag=KU_NULL; v.as.i=0; return v; }
static KuValue ku_v_int(int64_t i) { KuValue v; v.tag=KU_INT; v.as.i=i; return v; }
static KuValue ku_v_float(double f) { KuValue v; v.tag=KU_FLOAT; v.as.f=f; return v; }
static KuValue ku_v_bool(bool b) { KuValue v; v.tag=KU_BOOL; v.as.b=b; return v; }
static KuValue ku_v_str(KuString s) { KuValue v; v.tag=KU_STR; v.as.s=s; return v; }
static KuValue ku_v_object(KuObject* o) { KuValue v; v.tag=KU_OBJECT; v.as.o=o; return v; }
static KuValue ku_v_array(KuValueArray* a) { KuValue v; v.tag=KU_ARRAY; v.as.arr=a; return v; }
static KuValue ku_v_function(void* invoke, void* env) { KuValue v; v.tag=KU_FUNCTION; v.as.fn.invoke=invoke; v.as.fn.env=env; return v; }

/*
 * Stable, deliberately test-namespaced fault injection for the object ABI.
 * A site fails only on the selected 1-based ordinal, so recursive cleanup can
 * be exercised without replacing the process allocator or changing Ku APIs.
 */
typedef enum KuObjectAllocSite {
  KU_OBJECT_ALLOC_HEADER = 0,
  KU_OBJECT_ALLOC_ENTRIES,
  KU_OBJECT_ALLOC_REHASH,
  KU_OBJECT_ALLOC_VALUE_ARRAY_HEADER,
  KU_OBJECT_ALLOC_VALUE_ARRAY_DATA,
  KU_OBJECT_ALLOC_VALUE_ARRAY_GROW,
  KU_OBJECT_ALLOC_STRING_CLONE,
  KU_OBJECT_ALLOC_STRING_CONCAT,
  KU_OBJECT_ALLOC_SITE_COUNT
} KuObjectAllocSite;

#if KU_NATIVE_TEST_OBJECT_OOM
static KuAtomicRefcount ku_object_test_alloc_counts[KU_OBJECT_ALLOC_SITE_COUNT];

static const char* ku_object_alloc_site_name(KuObjectAllocSite site) {
  switch (site) {
    case KU_OBJECT_ALLOC_HEADER: return "object_header";
    case KU_OBJECT_ALLOC_ENTRIES: return "object_entries";
    case KU_OBJECT_ALLOC_REHASH: return "object_rehash";
    case KU_OBJECT_ALLOC_VALUE_ARRAY_HEADER: return "value_array_header";
    case KU_OBJECT_ALLOC_VALUE_ARRAY_DATA: return "value_array_data";
    case KU_OBJECT_ALLOC_VALUE_ARRAY_GROW: return "value_array_grow";
    case KU_OBJECT_ALLOC_STRING_CLONE: return "string_clone";
    case KU_OBJECT_ALLOC_STRING_CONCAT: return "string_concat";
    default: return "invalid";
  }
}

static size_t ku_object_test_next_alloc_count(KuObjectAllocSite site) {
  KuAtomicRefcount* counter = &ku_object_test_alloc_counts[(size_t)site];
  size_t current = ku_atomic_refcount_load(counter);
  for (;;) {
    if (current >= KU_REFCOUNT_MAX) return current;
    size_t expected = current;
    if (ku_atomic_refcount_compare_exchange_relaxed(counter, &expected, current + 1)) {
      return current + 1;
    }
    current = expected;
  }
}

static bool ku_object_test_alloc_should_fail(KuObjectAllocSite site) {
  const char* selected = getenv("KU_NATIVE_TEST_OBJECT_OOM_SITE");
  if (!selected || strcmp(selected, ku_object_alloc_site_name(site)) != 0) return false;
  size_t ordinal = 1;
  const char* text = getenv("KU_NATIVE_TEST_OBJECT_OOM_ORDINAL");
  if (text && *text) {
    char* end = NULL;
    unsigned long long parsed = strtoull(text, &end, 10);
    if (!end || *end != '\0' || parsed == 0
        || parsed > (unsigned long long)KU_REFCOUNT_MAX) return false;
    ordinal = (size_t)parsed;
  }
  return ku_object_test_next_alloc_count(site) == ordinal;
}

static void* ku_object_malloc(KuObjectAllocSite site, size_t bytes) {
  if (ku_object_test_alloc_should_fail(site)) return NULL;
  return malloc(bytes);
}

static void* ku_object_calloc(KuObjectAllocSite site, size_t count, size_t bytes) {
  if (ku_object_test_alloc_should_fail(site)) return NULL;
  return calloc(count, bytes);
}

static void* ku_object_realloc(KuObjectAllocSite site, void* old, size_t bytes) {
  if (ku_object_test_alloc_should_fail(site)) return NULL;
  return realloc(old, bytes);
}
#else
#define ku_object_malloc(site, bytes) malloc(bytes)
#define ku_object_calloc(site, count, bytes) calloc((count), (bytes))
#define ku_object_realloc(site, old, bytes) realloc((old), (bytes))
#endif

static KuError ku_object_out_of_memory_error(void) {
  return ku_error_make(
      ku_string_static((const uint8_t*)"object", 6),
      ku_string_static((const uint8_t*)"out_of_memory", 13),
      ku_string_static((const uint8_t*)"object allocation failed", 24));
}

static void ku_object_hard_fail_oom(void) {
  KuError error = ku_object_out_of_memory_error();
  ku_string_write(stderr, error.domain);
  fputc('/', stderr);
  ku_string_write(stderr, error.code);
  fputs(": ", stderr);
  ku_string_write(stderr, error.message);
  fputc('\n', stderr);
  exit(1);
}

static void ku_object_internal_fail(const char* message) {
  fprintf(stderr, "%s\n", message);
  exit(1);
}

static bool ku_object_string_try_clone(KuString value, KuString* out) {
  if (!out) ku_object_internal_fail("invalid object string clone output");
  *out = (KuString){0};
  if (value.storage == KU_STRING_STATIC) {
    *out = value;
    return true;
  }
  if (!value.ptr) return true;
  size_t capacity = value.len ? value.len : 1;
  uint8_t* data = (uint8_t*)ku_object_malloc(KU_OBJECT_ALLOC_STRING_CLONE, capacity);
  if (!data) return false;
  if (value.len) memcpy(data, value.ptr, value.len);
  *out = (KuString){ data, value.len, capacity, KU_STRING_OWNED };
  return true;
}

static bool ku_object_string_try_concat(KuString left, KuString right, KuString* out) {
  if (!out) ku_object_internal_fail("invalid object string concat output");
  *out = (KuString){0};
  if (right.len > SIZE_MAX - left.len) return false;
  size_t len = left.len + right.len;
  uint8_t* data = (uint8_t*)ku_object_malloc(
      KU_OBJECT_ALLOC_STRING_CONCAT, len ? len : 1);
  if (!data) return false;
  if (left.len) memcpy(data, left.ptr, left.len);
  if (right.len) memcpy(data + left.len, right.ptr, right.len);
  *out = (KuString){ data, len, len ? len : 1, KU_STRING_OWNED };
  return true;
}

static uint64_t ku_obj_hash(KuString key) {
  uint64_t hash = 1469598103934665603ULL;
  for (size_t index = 0; index < key.len; index++) {
    hash ^= key.ptr[index];
    hash *= 1099511628211ULL;
  }
  return hash;
}

static void ku_value_drop(KuValue* value);
static KuValue ku_value_move(KuValue* value);
static bool ku_value_try_clone(KuValue value, KuValue* out);
static void ku_object_drop(KuObject* object);
static bool ku_object_try_clone(KuObject* object, KuObject** out);
static void ku_value_array_drop(KuValueArray* array);
static bool ku_value_array_try_clone(KuValueArray* array, KuValueArray** out);
static bool ku_value_equal(KuValue left, KuValue right);

static bool ku_object_try_capacity(size_t requested, size_t* out) {
  size_t normalized = 8;
  while (normalized < requested) {
    if (normalized > SIZE_MAX / 2) return false;
    normalized *= 2;
  }
  if (normalized > SIZE_MAX / sizeof(KuEntry)) return false;
  *out = normalized;
  return true;
}

static bool ku_object_try_new(size_t requested, KuObject** out) {
  if (!out) ku_object_internal_fail("invalid object allocation output");
  *out = NULL;
  size_t capacity = 0;
  if (!ku_object_try_capacity(requested, &capacity)) return false;
  KuObject* object = (KuObject*)ku_object_malloc(
      KU_OBJECT_ALLOC_HEADER, sizeof(KuObject));
  if (!object) return false;
  KuEntry* entries = (KuEntry*)ku_object_calloc(
      KU_OBJECT_ALLOC_ENTRIES, capacity, sizeof(KuEntry));
  if (!entries) {
    free(object);
    return false;
  }
  object->len = 0;
  object->cap = capacity;
  object->entries = entries;
  *out = object;
  return true;
}

static KuObject* ku_object_new(size_t requested) {
  KuObject* object = NULL;
  if (!ku_object_try_new(requested, &object)) ku_object_hard_fail_oom();
  return object;
}

static void ku_object_require_valid(KuObject* object) {
  if (!object || !object->entries || object->cap < 8
      || (object->cap & (object->cap - 1)) != 0 || object->len >= object->cap) {
    ku_object_internal_fail("invalid object hash table");
  }
}

static void ku_object_insert_raw(
    KuEntry* entries, size_t capacity, KuString key, KuValue value) {
  size_t mask = capacity - 1;
  size_t index = (size_t)ku_obj_hash(key) & mask;
  for (size_t probes = 0; probes < capacity; probes++) {
    if (!entries[index].used) {
      entries[index].key = key;
      entries[index].value = value;
      entries[index].used = true;
      return;
    }
    index = (index + 1) & mask;
  }
  ku_object_internal_fail("object hash table probe exhausted");
}

static bool ku_object_try_rehash(KuObject* object) {
  ku_object_require_valid(object);
  if (object->cap > SIZE_MAX / 2) return false;
  size_t new_capacity = object->cap * 2;
  if (new_capacity > SIZE_MAX / sizeof(KuEntry)) return false;
  KuEntry* entries = (KuEntry*)ku_object_calloc(
      KU_OBJECT_ALLOC_REHASH, new_capacity, sizeof(KuEntry));
  if (!entries) return false;
  for (size_t index = 0; index < object->cap; index++) {
    if (object->entries[index].used) {
      ku_object_insert_raw(entries, new_capacity,
          object->entries[index].key, object->entries[index].value);
    }
  }
  KuEntry* old_entries = object->entries;
  object->entries = entries;
  object->cap = new_capacity;
  free(old_entries);
  return true;
}

/* On failure both key and value remain owned by the caller. */
static bool ku_object_try_set_owned(
    KuObject* object, KuString* key, KuValue* value) {
  ku_object_require_valid(object);
  if (!key || !value) ku_object_internal_fail("invalid object insertion value");
  size_t mask = object->cap - 1;
  size_t index = (size_t)ku_obj_hash(*key) & mask;
  for (size_t probes = 0; probes < object->cap; probes++) {
    if (!object->entries[index].used) break;
    if (ku_string_equal(object->entries[index].key, *key)) {
      ku_string_drop(key);
      ku_value_drop(&object->entries[index].value);
      object->entries[index].value = ku_value_move(value);
      return true;
    }
    index = (index + 1) & mask;
  }
  size_t threshold = object->cap - object->cap / 4;
  if (object->len == SIZE_MAX) return false;
  if (object->len + 1 >= threshold && !ku_object_try_rehash(object)) return false;
  KuString moved_key = ku_string_move(key);
  KuValue moved_value = ku_value_move(value);
  ku_object_insert_raw(object->entries, object->cap, moved_key, moved_value);
  object->len++;
  return true;
}

/* The key is borrowed. Replacing an existing entry neither allocates nor
 * touches the caller's key; inserting a new entry clones it before the owned
 * setter takes both payloads. On false, `value` remains owned by the caller. */
static bool ku_object_try_set_copy_key(
    KuObject* object, KuString key, KuValue* value) {
  ku_object_require_valid(object);
  if (!value) ku_object_internal_fail("invalid object insertion value");
  size_t mask = object->cap - 1;
  size_t index = (size_t)ku_obj_hash(key) & mask;
  for (size_t probes = 0; probes < object->cap; probes++) {
    if (!object->entries[index].used) break;
    if (ku_string_equal(object->entries[index].key, key)) {
      ku_value_drop(&object->entries[index].value);
      object->entries[index].value = ku_value_move(value);
      return true;
    }
    index = (index + 1) & mask;
  }
  KuString owned_key = (KuString){0};
  if (!ku_object_string_try_clone(key, &owned_key)) return false;
  if (ku_object_try_set_owned(object, &owned_key, value)) return true;
  ku_string_drop(&owned_key);
  return false;
}

static void ku_object_set(KuObject* object, KuString key, KuValue value) {
  if (!ku_object_try_set_owned(object, &key, &value)) {
    ku_string_drop(&key);
    ku_value_drop(&value);
    ku_object_hard_fail_oom();
  }
}

static KuValue* ku_object_get(KuObject* object, KuString key) {
  if (!object || !object->entries || object->cap == 0
      || (object->cap & (object->cap - 1)) != 0) return NULL;
  size_t mask = object->cap - 1;
  size_t index = (size_t)ku_obj_hash(key) & mask;
  for (size_t probes = 0; probes < object->cap; probes++) {
    if (!object->entries[index].used) return NULL;
    if (ku_string_equal(object->entries[index].key, key)) {
      return &object->entries[index].value;
    }
    index = (index + 1) & mask;
  }
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}

static bool ku_value_array_try_new(size_t capacity, KuValueArray** out) {
  if (!out) ku_object_internal_fail("invalid value array allocation output");
  *out = NULL;
  if (capacity > SIZE_MAX / sizeof(KuValue)) return false;
  KuValueArray* array = (KuValueArray*)ku_object_malloc(
      KU_OBJECT_ALLOC_VALUE_ARRAY_HEADER, sizeof(KuValueArray));
  if (!array) return false;
  KuValue* data = NULL;
  if (capacity != 0) {
    data = (KuValue*)ku_object_malloc(
        KU_OBJECT_ALLOC_VALUE_ARRAY_DATA, capacity * sizeof(KuValue));
    if (!data) {
      free(array);
      return false;
    }
  }
  array->len = 0;
  array->cap = capacity;
  array->data = data;
  *out = array;
  return true;
}

static KuValueArray* ku_value_array_new(void) {
  KuValueArray* array = NULL;
  if (!ku_value_array_try_new(0, &array)) ku_object_hard_fail_oom();
  return array;
}

static bool ku_value_array_try_reserve(KuValueArray* array, size_t need) {
  if (!array || array->len > array->cap || (array->cap && !array->data)) {
    ku_object_internal_fail("invalid value array");
  }
  if (need <= array->cap) return true;
  size_t capacity = array->cap;
  if (capacity == 0) capacity = 8;
  while (capacity < need) {
    if (capacity > SIZE_MAX / 2) {
      capacity = need;
      break;
    }
    capacity *= 2;
  }
  if (capacity < need || capacity > SIZE_MAX / sizeof(KuValue)) return false;
  KuValue* data = (KuValue*)ku_object_realloc(
      KU_OBJECT_ALLOC_VALUE_ARRAY_GROW, array->data,
      capacity * sizeof(KuValue));
  if (!data) return false;
  array->data = data;
  array->cap = capacity;
  return true;
}

/* On failure value remains owned by the caller. */
static bool ku_value_array_try_push_owned(KuValueArray* array, KuValue* value) {
  if (!value) ku_object_internal_fail("invalid value array element");
  if (!array || array->len == SIZE_MAX) return false;
  size_t need = array->len + 1;
  if (!ku_value_array_try_reserve(array, need)) return false;
  array->data[array->len++] = ku_value_move(value);
  return true;
}

static void ku_value_array_push(KuValueArray* array, KuValue value) {
  if (!ku_value_array_try_push_owned(array, &value)) {
    ku_value_drop(&value);
    ku_object_hard_fail_oom();
  }
}

static void ku_object_drop(KuObject* object) {
  if (!object) return;
  if (object->entries) {
    for (size_t index = 0; index < object->cap; index++) {
      if (object->entries[index].used) {
        ku_string_drop(&object->entries[index].key);
        ku_value_drop(&object->entries[index].value);
      }
    }
  }
  free(object->entries);
  free(object);
}

static void ku_value_array_drop(KuValueArray* array) {
  if (!array) return;
  for (size_t index = 0; index < array->len; index++) {
    ku_value_drop(&array->data[index]);
  }
  free(array->data);
  free(array);
}

static KuObject* ku_object_move(KuObject** object) {
  KuObject* moved = *object;
  *object = NULL;
  return moved;
}

static KuValue ku_value_move(KuValue* value) {
  KuValue moved = *value;
  value->tag = KU_NULL;
  value->as.i = 0;
  return moved;
}

static void ku_value_drop(KuValue* value) {
  if (!value) return;
  switch (value->tag) {
    case KU_STR: ku_string_drop(&value->as.s); break;
    case KU_OBJECT: ku_object_drop(value->as.o); value->as.o = NULL; break;
    case KU_ARRAY: ku_value_array_drop(value->as.arr); value->as.arr = NULL; break;
    case KU_FUNCTION:
      if (value->as.fn.env) ((KuEnvHeader*)value->as.fn.env)->release(value->as.fn.env);
      value->as.fn.env = NULL;
      break;
    default: break;
  }
  value->tag = KU_NULL;
  value->as.i = 0;
}

static bool ku_object_try_clone(KuObject* object, KuObject** out) {
  if (!out) ku_object_internal_fail("invalid object clone output");
  *out = NULL;
  if (!object) return true;
  ku_object_require_valid(object);
  KuObject* cloned = NULL;
  if (!ku_object_try_new(object->cap, &cloned)) return false;
  for (size_t index = 0; index < object->cap; index++) {
    if (!object->entries[index].used) continue;
    KuString key = (KuString){0};
    KuValue value = ku_v_null();
    if (!ku_object_string_try_clone(object->entries[index].key, &key)
        || !ku_value_try_clone(object->entries[index].value, &value)
        || !ku_object_try_set_owned(cloned, &key, &value)) {
      ku_string_drop(&key);
      ku_value_drop(&value);
      ku_object_drop(cloned);
      return false;
    }
  }
  *out = cloned;
  return true;
}

static bool ku_value_array_try_clone(KuValueArray* array, KuValueArray** out) {
  if (!out) ku_object_internal_fail("invalid value array clone output");
  *out = NULL;
  if (!array) return true;
  if (array->len > array->cap || (array->len && !array->data)) {
    ku_object_internal_fail("invalid value array clone source");
  }
  KuValueArray* cloned = NULL;
  if (!ku_value_array_try_new(array->len, &cloned)) return false;
  for (size_t index = 0; index < array->len; index++) {
    KuValue value = ku_v_null();
    if (!ku_value_try_clone(array->data[index], &value)
        || !ku_value_array_try_push_owned(cloned, &value)) {
      ku_value_drop(&value);
      ku_value_array_drop(cloned);
      return false;
    }
  }
  *out = cloned;
  return true;
}

static bool ku_value_try_clone(KuValue value, KuValue* out) {
  if (!out) ku_object_internal_fail("invalid value clone output");
  *out = ku_v_null();
  switch (value.tag) {
    case KU_STR: {
      KuString string = (KuString){0};
      if (!ku_object_string_try_clone(value.as.s, &string)) return false;
      *out = ku_v_str(string);
      return true;
    }
    case KU_OBJECT: {
      KuObject* object = NULL;
      if (!ku_object_try_clone(value.as.o, &object)) return false;
      *out = ku_v_object(object);
      return true;
    }
    case KU_ARRAY: {
      KuValueArray* array = NULL;
      if (!ku_value_array_try_clone(value.as.arr, &array)) return false;
      *out = ku_v_array(array);
      return true;
    }
    case KU_FUNCTION:
      if (value.as.fn.env) ((KuEnvHeader*)value.as.fn.env)->retain(value.as.fn.env);
      *out = value;
      return true;
    default:
      *out = value;
      return true;
  }
}

static KuObject* ku_object_clone(KuObject* object) {
  KuObject* cloned = NULL;
  if (!ku_object_try_clone(object, &cloned)) ku_object_hard_fail_oom();
  return cloned;
}

static KuValueArray* ku_value_array_clone(KuValueArray* array) {
  KuValueArray* cloned = NULL;
  if (!ku_value_array_try_clone(array, &cloned)) ku_object_hard_fail_oom();
  return cloned;
}

static KuValue ku_value_clone(KuValue value) {
  KuValue cloned = ku_v_null();
  if (!ku_value_try_clone(value, &cloned)) ku_object_hard_fail_oom();
  return cloned;
}

static void ku_value_print(KuValue value) {
  switch (value.tag) {
    case KU_INT: printf("%lld", (long long)value.as.i); break;
    case KU_FLOAT: printf("%.17g", value.as.f); break;
    case KU_BOOL: printf("%s", value.as.b ? "true" : "false"); break;
    case KU_STR: ku_string_write(stdout, value.as.s); break;
    case KU_OBJECT: printf("[object]"); break;
    case KU_ARRAY: printf("[array]"); break;
    case KU_FUNCTION: printf("<function>"); break;
    default: printf("null"); break;
  }
}

static KuValue ku_object_get_or(KuObject* object, KuString key, KuValue fallback) {
  KuValue* found = ku_object_get(object, key);
  if (!found) return fallback;
  KuValue cloned = ku_v_null();
  if (!ku_value_try_clone(*found, &cloned)) {
    ku_value_drop(&fallback);
    ku_object_hard_fail_oom();
  }
  ku_value_drop(&fallback);
  return cloned;
}

static bool ku_value_array_equal(KuValueArray* left, KuValueArray* right) {
  if (!left || !right) return left == right;
  if (left->len != right->len) return false;
  for (size_t index = 0; index < left->len; index++) {
    if (!ku_value_equal(left->data[index], right->data[index])) return false;
  }
  return true;
}

static bool ku_object_equal(KuObject* left, KuObject* right) {
  if (!left || !right) return left == right;
  if (left->len != right->len) return false;
  for (size_t index = 0; index < left->cap; index++) {
    if (left->entries[index].used) {
      KuValue* found = ku_object_get(right, left->entries[index].key);
      if (!found || !ku_value_equal(left->entries[index].value, *found)) return false;
    }
  }
  return true;
}

static bool ku_value_equal(KuValue left, KuValue right) {
  if (left.tag != right.tag) return false;
  switch (left.tag) {
    case KU_NULL: return true;
    case KU_INT: return left.as.i == right.as.i;
    case KU_FLOAT: return left.as.f == right.as.f;
    case KU_BOOL: return left.as.b == right.as.b;
    case KU_STR: return ku_string_equal(left.as.s, right.as.s);
    case KU_OBJECT: return ku_object_equal(left.as.o, right.as.o);
    case KU_ARRAY: return ku_value_array_equal(left.as.arr, right.as.arr);
    case KU_FUNCTION: return false;
    default: return false;
  }
}

"#
    );
    Ok(())
}

/// Emit the consuming bridge from a homogeneous native `KuArray_E` into the
/// heterogeneous `KuValueArray` used by dynamic objects and JSON. Only array
/// types that actually reach a KuValue-wrapping call site are emitted; scanning
/// every array in an object/HTTP program would make an unrelated `[Struct]` or
/// `[Closure]` fail code generation even when it is never boxed.
fn emit_kuvalue_array_wrappers(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    if !program_uses_object(program) {
        return Ok(());
    }

    let mut element_types = Vec::new();
    collect_kuvalue_array_elements_program(program, &mut element_types);
    for element in &element_types {
        if !kuvalue_array_element_supported(element) {
            return Err(unsupported(format!(
                "native dynamic object cannot hold array elements of type {element}"
            )));
        }

        let suffix = c_type_suffix(element)?;
        let array_type = c_array_type(element)?;
        // Moving every owned slot clears it before the typed array drop below.
        // The drop can therefore release the old backing buffer without freeing
        // strings/objects/nested arrays that now belong to KuValueArray.
        let moved = c_move_value(element, "array.data[index]")?;
        let wrap_element = if let IrType::Array(inner) = element {
            format!(
                "    KuValue element_value = ku_v_null();\n\
                     if (!ku_try_v_typed_array_{}({}, &element_value)) {{\n\
                       ku_value_array_drop(boxed);\n\
                       ku_array_drop_{suffix}(&array);\n\
                       return false;\n\
                     }}\n",
                c_type_suffix(inner)?,
                moved
            )
        } else {
            format!(
                "    KuValue element_value = {};\n",
                ku_value_wrap(element, &moved)?
            )
        };
        out.push_str(&format!(
            "static bool ku_try_v_typed_array_{suffix}({array_type} array, KuValue* out) {{\n\
             \x20 if (!out) ku_object_internal_fail(\"invalid typed array conversion output\");\n\
             \x20 *out = ku_v_null();\n\
             \x20 KuValueArray* boxed = NULL;\n\
             \x20 if (!ku_value_array_try_new(array.len, &boxed)) {{\n\
             \x20   ku_array_drop_{suffix}(&array);\n\
             \x20   return false;\n\
             \x20 }}\n\
             \x20 for (size_t index = 0; index < array.len; index++) {{\n\
             {wrap_element}\
             \x20   if (!ku_value_array_try_push_owned(boxed, &element_value)) {{\n\
             \x20     ku_value_drop(&element_value);\n\
             \x20     ku_value_array_drop(boxed);\n\
             \x20     ku_array_drop_{suffix}(&array);\n\
             \x20     return false;\n\
             \x20   }}\n\
             \x20 }}\n\
             \x20 ku_array_drop_{suffix}(&array);\n\
             \x20 *out = ku_v_array(boxed);\n\
             \x20 return true;\n\
             }}\n\
             static KuValue ku_v_typed_array_{suffix}({array_type} array) {{\n\
             \x20 KuValue value = ku_v_null();\n\
             \x20 if (!ku_try_v_typed_array_{suffix}(array, &value)) ku_object_hard_fail_oom();\n\
             \x20 return value;\n\
             }}\n"
        ));
    }
    if !element_types.is_empty() {
        out.push('\n');
    }
    Ok(())
}

/// Emit allocation-free equality adapters between a dynamic KuValue array and
/// each homogeneous typed-array ABI that is actually compared with one. This is
/// intentionally separate from the consuming boxing bridge above: equality is
/// a borrow and must leave both operands usable.
fn emit_kuvalue_typed_array_equality_helpers(
    out: &mut COutput,
    program: &IrProgram,
) -> KuResult<()> {
    out.check()?;
    if !program_uses_object(program) {
        return Ok(());
    }

    let mut element_types = Vec::new();
    collect_kuvalue_equality_array_elements_program(program, &mut element_types);
    for element in &element_types {
        let suffix = c_type_suffix(element)?;
        let array_type = c_array_type(element)?;
        let dynamic_element = "dynamic.as.arr->data[index]";
        let typed_element = "typed.data[index]";
        let equal = if let IrType::Array(inner) = element {
            format!(
                "ku_value_equal_typed_array_{}({dynamic_element}, {typed_element})",
                c_type_suffix(inner)?
            )
        } else if matches!(element, IrType::Closure { .. }) {
            // Runtime function values are never equal, even to themselves.
            "false".to_string()
        } else if let Some(wrapped) = ku_value_borrow_wrap(element, typed_element)? {
            format!("ku_value_equal({dynamic_element}, {wrapped})")
        } else {
            // KuValue deliberately has no tag for struct/enum/result/etc. Empty
            // arrays still compare equal by length; every actual element differs.
            "false".to_string()
        };
        out.push_str(&format!(
            "static bool ku_value_equal_typed_array_{suffix}(KuValue dynamic, {array_type} typed) {{\n\
             \x20 if (dynamic.tag != KU_ARRAY || !dynamic.as.arr || dynamic.as.arr->len != typed.len) return false;\n\
             \x20 for (size_t index = 0; index < typed.len; index++) if (!({equal})) return false;\n\
             \x20 return true;\n\
             }}\n"
        ));
    }
    if !element_types.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn collect_kuvalue_equality_array_elements_program(program: &IrProgram, output: &mut Vec<IrType>) {
    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                walk_inst_exprs(inst, &mut |expr| {
                    collect_kuvalue_equality_array_elements_expr(expr, output)
                });
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                collect_kuvalue_equality_array_elements_expr(expr, output)
            });
        }
    }
}

fn collect_kuvalue_equality_array_elements_expr(expr: &IrExpr, output: &mut Vec<IrType>) {
    if let IrExprKind::Binary { left, op, right } = &expr.kind {
        if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) {
            let typed = if ir_type_is_ku_value(&left.ty) {
                Some(right.as_ref())
            } else if ir_type_is_ku_value(&right.ty) {
                Some(left.as_ref())
            } else {
                None
            };
            if let Some(typed) = typed {
                collect_kuvalue_array_element_type(&typed.ty, output);
            }
        }
    }
    for child in expr_children(expr) {
        collect_kuvalue_equality_array_elements_expr(child, output);
    }
}

/// Find the exact types passed to `ku_value_wrap`: object-literal values,
/// and `object.get_or` defaults. JSON uses borrowed typed writers. Nested array element
/// types are recorded post-order so an outer converter can call an already
/// declared inner converter without forward declarations.
fn collect_kuvalue_array_elements_program(program: &IrProgram, output: &mut Vec<IrType>) {
    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                // A dynamic-object Store boxes its RHS exactly like an object
                // literal. Record typed array RHS values so its fallible bridge
                // is emitted even when no other object/json expression uses the
                // same array element type.
                if let IrInst::Store { target, value } = inst {
                    if dynamic_object_store_target(target) {
                        collect_kuvalue_array_element_type(&value.ty, output);
                    }
                }
                walk_inst_exprs(inst, &mut |expr| {
                    collect_kuvalue_array_elements_expr(expr, output)
                });
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                collect_kuvalue_array_elements_expr(expr, output)
            });
        }
    }
}

fn collect_kuvalue_array_elements_expr(expr: &IrExpr, output: &mut Vec<IrType>) {
    if let IrExprKind::Call {
        kind: IrCallKind::Intrinsic(name),
        args,
        ..
    } = &expr.kind
    {
        match name.as_str() {
            "__ku_object" => {
                for value in args.iter().skip(1).step_by(2) {
                    collect_kuvalue_array_element_type(&value.ty, output);
                }
            }
            "object.get_or" => {
                if let Some(default) = args.get(2) {
                    collect_kuvalue_array_element_type(&default.ty, output);
                }
            }
            _ => {}
        }
    }
    for child in expr_children(expr) {
        collect_kuvalue_array_elements_expr(child, output);
    }
}

fn collect_kuvalue_array_element_type(ty: &IrType, output: &mut Vec<IrType>) {
    let IrType::Array(element) = ty else {
        return;
    };
    collect_kuvalue_array_element_type(element, output);
    if !output.contains(element.as_ref()) {
        output.push(*element.clone());
    }
}

fn kuvalue_array_element_supported(ty: &IrType) -> bool {
    match ty {
        IrType::Int | IrType::Float | IrType::Bool | IrType::Str | IrType::Null => true,
        IrType::Named(name) => name == "__ku_object" || name == "__ku_value",
        IrType::Array(inner) => kuvalue_array_element_supported(inner),
        _ => false,
    }
}

/// Emit `ku_object_get_result` (strict `obj[key]` -> Result&lt;KuValue&gt;) after the
/// result ABI, since it depends on `KuResult_kuvalue`. Missing keys produce
/// `Err{domain:"object", code:"missing_key", message:"missing object key: <key>"}`.
fn emit_object_result_helpers(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    if !program_uses_object(program) {
        return Ok(());
    }
    out.push_str(
        r#"static KuResult_kuvalue ku_object_out_of_memory_result(void) {
  return (KuResult_kuvalue){ false, ku_v_null(), ku_object_out_of_memory_error() };
}

static KuResult_kuvalue ku_object_get_result(KuObject* object, KuString key) {
  KuValue* found = ku_object_get(object, key);
  if (found) {
    KuValue cloned = ku_v_null();
    if (!ku_value_try_clone(*found, &cloned)) return ku_object_out_of_memory_result();
    return (KuResult_kuvalue){ true, cloned, (KuError){0} };
  }
  KuString message = (KuString){0};
  if (!ku_object_string_try_concat(
          ku_string_static((const uint8_t*)"missing object key: ", 20),
          key,
          &message)) {
    return ku_object_out_of_memory_result();
  }
  return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(
      ku_string_static((const uint8_t*)"object", 6),
      ku_string_static((const uint8_t*)"missing_key", 11),
      message) };
}

static KuResult_int ku_value_as_int(KuValue value) {
  if (value.tag == KU_INT) {
    int64_t integer = value.as.i;
    ku_value_drop(&value);
    return (KuResult_int){ true, integer, (KuError){0} };
  }
  ku_value_drop(&value);
  return (KuResult_int){ false, 0, ku_error_make(
      ku_string_static((const uint8_t*)"value", 5),
      ku_string_static((const uint8_t*)"type_mismatch", 13),
      ku_string_static((const uint8_t*)"expected int value", 18)) };
}

static KuResult_str ku_value_as_str(KuValue value) {
  if (value.tag == KU_STR) {
    KuString string = value.as.s;
    value.tag = KU_NULL;
    value.as.i = 0;
    return (KuResult_str){ true, string, (KuError){0} };
  }
  ku_value_drop(&value);
  return (KuResult_str){ false, (KuString){0}, ku_error_make(
      ku_string_static((const uint8_t*)"value", 5),
      ku_string_static((const uint8_t*)"type_mismatch", 13),
      ku_string_static((const uint8_t*)"expected str value", 18)) };
}

static KuResult_kuvalue ku_value_get_result(KuValue value, KuString key) {
  if (value.tag == KU_OBJECT) return ku_object_get_result(value.as.o, key);
  return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(
      ku_string_static((const uint8_t*)"object", 6),
      ku_string_static((const uint8_t*)"type_unsupported", 16),
      ku_string_static((const uint8_t*)"expected object value", 21)) };
}

static KuResult_kuvalue ku_value_index_result(KuValue value, int64_t index) {
  if (value.tag != KU_ARRAY) {
    return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(
        ku_string_static((const uint8_t*)"array", 5),
        ku_string_static((const uint8_t*)"not_an_array", 12),
        ku_string_static((const uint8_t*)"expected array value", 20)) };
  }
  KuValueArray* array = value.as.arr;
  if (array && index >= 0 && (size_t)index < array->len) {
    KuValue cloned = ku_v_null();
    if (!ku_value_try_clone(array->data[index], &cloned)) {
      return ku_object_out_of_memory_result();
    }
    return (KuResult_kuvalue){ true, cloned, (KuError){0} };
  }
  char number[32];
  int length = snprintf(number, sizeof(number), "%lld", (long long)index);
  if (length < 0 || (size_t)length >= sizeof(number)) {
    return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(
        ku_string_static((const uint8_t*)"array", 5),
        ku_string_static((const uint8_t*)"index_out_of_bounds", 19),
        ku_string_static((const uint8_t*)"array index out of bounds", 25)) };
  }
  KuString message = (KuString){0};
  if (!ku_object_string_try_concat(
          ku_string_static((const uint8_t*)"array index out of bounds: ", 27),
          ku_string_static((const uint8_t*)number, (size_t)length),
          &message)) {
    return ku_object_out_of_memory_result();
  }
  return (KuResult_kuvalue){ false, ku_v_null(), ku_error_make(
      ku_string_static((const uint8_t*)"array", 5),
      ku_string_static((const uint8_t*)"index_out_of_bounds", 19),
      message) };
}

"#,
    );
    Ok(())
}

/// Record the exact static values passed to `json.stringify`. Unlike dynamic
/// object construction, JSON accepts user structs and must reject unsupported
/// values as a Result at runtime rather than aborting C generation.
fn collect_json_stringify_root_types(program: &IrProgram, output: &mut Vec<IrType>) {
    fn visit(expr: &IrExpr, output: &mut Vec<IrType>) {
        if let IrExprKind::Call {
            kind: IrCallKind::Intrinsic(name),
            args,
            ..
        } = &expr.kind
        {
            if name == "json.stringify" {
                if let Some(value) = args.first() {
                    if !output.contains(&value.ty) {
                        output.push(value.ty.clone());
                    }
                }
            }
        }
        for child in expr_children(expr) {
            visit(child, output);
        }
    }

    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                walk_inst_exprs(inst, &mut |expr| visit(expr, output));
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| visit(expr, output));
        }
    }
}

fn json_struct_layout<'a>(program: &'a IrProgram, name: &str) -> Option<&'a IrStructLayout> {
    program
        .layouts
        .structs
        .iter()
        .find(|layout| layout.name == name)
}

/// JSON typed writers borrow their input; the consuming wrapper drops the root
/// exactly once after success or failure. Gather aggregates recursively and use
/// a visited set so a legal `Node { children: [Node] }` type cannot recurse in
/// the Rust emitter. C prototypes are emitted for the whole set before bodies.
fn collect_json_writer_types(ty: &IrType, program: &IrProgram, output: &mut Vec<IrType>) {
    match ty {
        IrType::Array(element) => {
            if output.contains(ty) {
                return;
            }
            output.push(ty.clone());
            collect_json_writer_types(element, program, output);
        }
        IrType::Named(name) if name == "__ku_time" || name == "__ku_error_type" => {
            if !output.contains(ty) {
                output.push(ty.clone());
            }
        }
        IrType::Named(name) => {
            let Some(layout) = json_struct_layout(program, name) else {
                return;
            };
            if output.contains(ty) {
                return;
            }
            output.push(ty.clone());
            for field in &layout.fields {
                collect_json_writer_types(&field.ty, program, output);
            }
        }
        _ => {}
    }
}

fn json_typed_root_required(ty: &IrType) -> bool {
    match ty {
        IrType::Array(_) | IrType::Result(_) => true,
        IrType::Named(name) => name != "__ku_object" && name != "__ku_value",
        _ => false,
    }
}

fn json_unsupported_type_name(ty: &IrType) -> &'static str {
    match ty {
        IrType::Result(_) => "result",
        IrType::Closure { .. } | IrType::Function => "function",
        IrType::Named(name) if enum_type_name(name).is_some() => "enum",
        IrType::Named(name) if name == "__ku_http_server" => "http listener",
        IrType::Cell(_) => "task",
        _ => "value",
    }
}

/// Produce a borrow-only writer call for one typed value. Unsupported variants
/// intentionally lower to a runtime `json/stringify_error`; ownership remains
/// with the root wrapper and is released by its ordinary deep-drop path.
fn json_typed_write_call(
    ty: &IrType,
    expression: &str,
    depth: &str,
    output: &str,
    error: &str,
    program: &IrProgram,
) -> KuResult<String> {
    match ty {
        IrType::Int => Ok(format!(
            "ku_json_write_value({output}, ku_v_int({expression}), {depth}, {error})"
        )),
        IrType::Float => Ok(format!(
            "ku_json_write_value({output}, ku_v_float({expression}), {depth}, {error})"
        )),
        IrType::Bool => Ok(format!(
            "ku_json_write_value({output}, ku_v_bool({expression}), {depth}, {error})"
        )),
        IrType::Str => Ok(format!(
            "ku_json_write_value({output}, ku_v_str({expression}), {depth}, {error})"
        )),
        IrType::Null => Ok(format!(
            "ku_json_write_value({output}, ku_v_null(), {depth}, {error})"
        )),
        IrType::Array(_) => Ok(format!(
            "ku_json_write_typed_{}({output}, {expression}, {depth}, {error})",
            c_type_suffix(ty)?
        )),
        IrType::Named(name) if name == "__ku_object" => Ok(format!(
            "ku_json_write_value({output}, ku_v_object({expression}), {depth}, {error})"
        )),
        IrType::Named(name) if name == "__ku_value" => Ok(format!(
            "ku_json_write_value({output}, {expression}, {depth}, {error})"
        )),
        IrType::Named(name) if name == "__ku_time" || name == "__ku_error_type" => Ok(format!(
            "ku_json_write_typed_{}({output}, {expression}, {depth}, {error})",
            c_type_suffix(ty)?
        )),
        IrType::Named(name) if json_struct_layout(program, name).is_some() => Ok(format!(
            "ku_json_write_typed_{}({output}, {expression}, {depth}, {error})",
            c_type_suffix(ty)?
        )),
        _ => Ok(format!(
            "ku_json_write_unsupported({depth}, {error}, \"json.stringify does not support {}\")",
            json_unsupported_type_name(ty)
        )),
    }
}

fn emit_json_typed_writer(out: &mut COutput, ty: &IrType, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let suffix = c_type_suffix(ty)?;
    let value_type = c_type(ty)?;
    out.push_str(&format!(
        "static bool ku_json_write_typed_{suffix}(KuJsonBuffer* output, {value_type} value, size_t depth, KuError* error) {{\n\
         \x20 if (depth > KU_JSON_MAX_DEPTH) return ku_json_fail(error, \"stringify_error\", \"json value nesting is too deep\");\n"
    ));

    match ty {
        IrType::Array(element) => {
            let element_call = json_typed_write_call(
                element,
                "value.data[index]",
                "depth + 1",
                "output",
                "error",
                program,
            )?;
            out.push_str(
                "  if (value.len && !value.data) return ku_json_fail(error, \"stringify_error\", \"invalid native array\");\n\
                 \x20 if (!ku_json_write_byte(output, '[', error)) return false;\n\
                 \x20 for (size_t index = 0; index < value.len; index++) {\n\
                 \x20   if (index != 0 && !ku_json_write_byte(output, ',', error)) return false;\n",
            );
            out.push_str(&format!("    if (!{element_call}) return false;\n"));
            out.push_str("  }\n  return ku_json_write_byte(output, ']', error);\n}\n");
        }
        IrType::Named(name) if name == "__ku_time" => {
            out.push_str(
                "  ku_time_validate(value);\n\
                 \x20 if (!ku_json_write_byte(output, '{', error)\n\
                 \x20     || !ku_json_write_string(output, ku_string_static((const uint8_t*)\"kind\", 4), error)\n\
                 \x20     || !ku_json_write_byte(output, ':', error)\n\
                 \x20     || !ku_json_write_value(output, ku_v_str(ku_time_kind(value)), depth + 1, error)\n\
                 \x20     || !ku_json_write_byte(output, ',', error)\n\
                 \x20     || !ku_json_write_string(output, ku_string_static((const uint8_t*)\"millis\", 6), error)\n\
                 \x20     || !ku_json_write_byte(output, ':', error)\n\
                 \x20     || !ku_json_write_value(output, ku_v_int(value.millis), depth + 1, error)) return false;\n\
                 \x20 return ku_json_write_byte(output, '}', error);\n}\n",
            );
        }
        IrType::Named(name) if name == "__ku_error_type" => {
            out.push_str(
                "  if (!ku_json_write_byte(output, '{', error)\n\
                 \x20     || !ku_json_write_string(output, ku_string_static((const uint8_t*)\"code\", 4), error)\n\
                 \x20     || !ku_json_write_byte(output, ':', error)\n\
                 \x20     || !ku_json_write_value(output, ku_v_str(value.code), depth + 1, error)\n\
                 \x20     || !ku_json_write_byte(output, ',', error)\n\
                 \x20     || !ku_json_write_string(output, ku_string_static((const uint8_t*)\"domain\", 6), error)\n\
                 \x20     || !ku_json_write_byte(output, ':', error)\n\
                 \x20     || !ku_json_write_value(output, ku_v_str(value.domain), depth + 1, error)\n\
                 \x20     || !ku_json_write_byte(output, ',', error)\n\
                 \x20     || !ku_json_write_string(output, ku_string_static((const uint8_t*)\"message\", 7), error)\n\
                 \x20     || !ku_json_write_byte(output, ':', error)\n\
                 \x20     || !ku_json_write_value(output, ku_v_str(value.message), depth + 1, error)) return false;\n\
                 \x20 return ku_json_write_byte(output, '}', error);\n}\n",
            );
        }
        IrType::Named(name) => {
            let layout = json_struct_layout(program, name).ok_or_else(|| {
                unsupported(format!(
                    "native JSON writer cannot find struct layout '{name}'"
                ))
            })?;
            let mut fields = layout.fields.iter().collect::<Vec<_>>();
            fields.sort_by(|left, right| left.name.cmp(&right.name));
            out.push_str("  if (!ku_json_write_byte(output, '{', error)) return false;\n");
            for (index, field) in fields.iter().enumerate() {
                if index != 0 {
                    out.push_str("  if (!ku_json_write_byte(output, ',', error)) return false;\n");
                }
                let key = c_static_utf8_string(&field.name);
                let field_expression = format!("value.{}", c_ident(&field.name));
                let field_call = json_typed_write_call(
                    &field.ty,
                    &field_expression,
                    "depth + 1",
                    "output",
                    "error",
                    program,
                )?;
                out.push_str(&format!(
                    "  if (!ku_json_write_string(output, {key}, error)\n\
                     \x20     || !ku_json_write_byte(output, ':', error)\n\
                     \x20     || !{field_call}) return false;\n"
                ));
            }
            out.push_str("  return ku_json_write_byte(output, '}', error);\n}\n");
        }
        _ => {
            return Err(unsupported(format!(
                "native JSON typed writer does not support {ty}"
            )))
        }
    }
    Ok(())
}

/// Emit non-consuming Result wrappers for every statically-typed JSON input.
/// Reuse the borrowed writers for arrays as well as structs: boxing an array
/// here would transfer its elements and free storage still owned by the caller.
fn emit_json_typed_stringify_helpers(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let mut roots = Vec::new();
    collect_json_stringify_root_types(program, &mut roots);
    let roots = roots
        .into_iter()
        .filter(json_typed_root_required)
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Ok(());
    }

    let mut writer_types = Vec::new();
    for root in &roots {
        collect_json_writer_types(root, program, &mut writer_types);
    }
    for ty in &writer_types {
        out.push_str(&format!(
            "static bool ku_json_write_typed_{}(KuJsonBuffer*, {}, size_t, KuError*);\n",
            c_type_suffix(ty)?,
            c_type(ty)?
        ));
    }
    if !writer_types.is_empty() {
        out.push('\n');
    }
    for ty in &writer_types {
        emit_json_typed_writer(out, ty, program)?;
    }

    for root in &roots {
        let suffix = c_type_suffix(root)?;
        let value_type = c_type(root)?;
        let write_call = json_typed_write_call(root, "value", "0", "&output", "&error", program)?;
        out.push_str(&format!(
            "static KuResult_str ku_json_stringify_typed_{suffix}({value_type} value) {{\n\
             \x20 KuJsonBuffer output = {{0}};\n\
             \x20 KuError error = (KuError){{0}};\n\
             \x20 bool ok = {write_call};\n\
             \x20 if (!ok) {{\n\
             \x20   ku_json_buffer_drop(&output);\n\
             \x20   if (!error.code.ptr) error = ku_json_error(\"stringify_error\", \"json stringify failed\");\n\
             \x20   return (KuResult_str){{ false, (KuString){{0}}, error }};\n\
             \x20 }}\n\
             \x20 ku_error_drop(&error);\n\
             \x20 return (KuResult_str){{ true, ku_json_buffer_take_string(&output), (KuError){{0}} }};\n\
             }}\n"
        ));
    }
    out.push('\n');
    Ok(())
}

fn emit_json_runtime(out: &mut COutput) {
    if out.failed() {
        return;
    }
    out.push_str(
        r#"
#define KU_JSON_MAX_INPUT_BYTES ((size_t)1000000)
#define KU_JSON_MAX_OUTPUT_BYTES ((size_t)1000000)
#define KU_JSON_MAX_DEPTH ((size_t)32)

typedef struct KuJsonBuffer {
  uint8_t* data;
  size_t len;
  size_t cap;
} KuJsonBuffer;

typedef struct KuJsonParser {
  const uint8_t* current;
  const uint8_t* end;
} KuJsonParser;

/*
 * Finite binary64 -> Rust `f64::to_string()` decimal.
 *
 * This is a bounded Dragon4-style interval search: it constructs the exact
 * binary value and its IEEE-754 rounding interval with a tiny base-1e9 bigint,
 * then tests at most 17 significant decimal digits.  Consequently formatting
 * does not depend on printf/strtod, the process locale, or the host floating
 * point rounding mode.  The final shortest mantissa is expanded to fixed
 * decimal notation because Rust Display for finite f64 does not use an
 * exponent.  The interval/tie rules follow the shortest-representation method
 * described by Ulf Adams, "Ryū: Fast Float-to-String Conversion" (PLDI 2018):
 * https://dl.acm.org/doi/10.1145/3192366.3192369
 * Rust's `core::num::flt2dec::strategy::dragon::format_shortest` compatibility
 * requires choosing the upper candidate when the two shortest decimals are
 * exactly equidistant.  No Ryū or Rust source code/table is copied here.
 *
 * Limits are structural, not data-dependent: 17 digit attempts, 58 binary
 * search steps per attempt, 64 bigint limbs, and at most 38 bigint operations
 * for either a power of two or a power of ten.
 */
#define KU_DTOA_BIG_BASE UINT32_C(1000000000)
#define KU_DTOA_BIG_LIMBS ((size_t)64)
#define KU_DTOA_TEXT_CAP ((size_t)384)

typedef struct KuDtoaBig {
  uint32_t limb[64];
  size_t len;
} KuDtoaBig;

static KuDtoaBig ku_dtoa_big_u64(uint64_t value) {
  KuDtoaBig result = {{0}, 0};
  while (value != 0) {
    result.limb[result.len++] = (uint32_t)(value % KU_DTOA_BIG_BASE);
    value /= KU_DTOA_BIG_BASE;
  }
  return result;
}

static bool ku_dtoa_big_mul_small(KuDtoaBig* value, uint32_t factor) {
  if (factor == 0 || value->len == 0) {
    *value = (KuDtoaBig){{0}, 0};
    return true;
  }
  uint64_t carry = 0;
  for (size_t index = 0; index < value->len; index++) {
    uint64_t product = (uint64_t)value->limb[index] * factor + carry;
    value->limb[index] = (uint32_t)(product % KU_DTOA_BIG_BASE);
    carry = product / KU_DTOA_BIG_BASE;
  }
  if (carry != 0) {
    if (value->len >= KU_DTOA_BIG_LIMBS) return false;
    value->limb[value->len++] = (uint32_t)carry;
  }
  return true;
}

static bool ku_dtoa_big_mul_pow2(KuDtoaBig* value, int exponent) {
  if (exponent < 0) return false;
  while (exponent >= 29) {
    if (!ku_dtoa_big_mul_small(value, UINT32_C(1) << 29)) return false;
    exponent -= 29;
  }
  return exponent == 0
      || ku_dtoa_big_mul_small(value, UINT32_C(1) << (unsigned)exponent);
}

static bool ku_dtoa_big_mul_pow10(KuDtoaBig* value, int exponent) {
  static const uint32_t small_powers[9] = {
    UINT32_C(1), UINT32_C(10), UINT32_C(100), UINT32_C(1000),
    UINT32_C(10000), UINT32_C(100000), UINT32_C(1000000),
    UINT32_C(10000000), UINT32_C(100000000)
  };
  if (exponent < 0) return false;
  while (exponent >= 9) {
    if (!ku_dtoa_big_mul_small(value, KU_DTOA_BIG_BASE)) return false;
    exponent -= 9;
  }
  return ku_dtoa_big_mul_small(value, small_powers[exponent]);
}

static bool ku_dtoa_big_mul_u64(
    const KuDtoaBig* left,
    uint64_t right,
    KuDtoaBig* output) {
  *output = (KuDtoaBig){{0}, 0};
  if (left->len == 0 || right == 0) return true;
  uint32_t factor[3] = {0, 0, 0};
  size_t factor_len = 0;
  while (right != 0) {
    factor[factor_len++] = (uint32_t)(right % KU_DTOA_BIG_BASE);
    right /= KU_DTOA_BIG_BASE;
  }
  if (left->len + factor_len > KU_DTOA_BIG_LIMBS) return false;
  output->len = left->len + factor_len;
  for (size_t left_index = 0; left_index < left->len; left_index++) {
    uint64_t carry = 0;
    for (size_t factor_index = 0; factor_index < factor_len; factor_index++) {
      size_t output_index = left_index + factor_index;
      uint64_t product = (uint64_t)left->limb[left_index] * factor[factor_index]
          + output->limb[output_index] + carry;
      output->limb[output_index] = (uint32_t)(product % KU_DTOA_BIG_BASE);
      carry = product / KU_DTOA_BIG_BASE;
    }
    size_t output_index = left_index + factor_len;
    while (carry != 0) {
      if (output_index >= KU_DTOA_BIG_LIMBS) return false;
      uint64_t sum = (uint64_t)output->limb[output_index] + carry;
      output->limb[output_index] = (uint32_t)(sum % KU_DTOA_BIG_BASE);
      carry = sum / KU_DTOA_BIG_BASE;
      output_index++;
      if (output_index > output->len) output->len = output_index;
    }
  }
  while (output->len != 0 && output->limb[output->len - 1] == 0) output->len--;
  return true;
}

static int ku_dtoa_big_compare(const KuDtoaBig* left, const KuDtoaBig* right) {
  if (left->len != right->len) return left->len < right->len ? -1 : 1;
  size_t index = left->len;
  while (index != 0) {
    index--;
    if (left->limb[index] != right->limb[index]) {
      return left->limb[index] < right->limb[index] ? -1 : 1;
    }
  }
  return 0;
}

/* Compare mantissa * 2^binary_exponent with 10^decimal_exponent. */
static bool ku_dtoa_compare_value_pow10(
    uint64_t mantissa,
    int binary_exponent,
    int decimal_exponent,
    int* comparison) {
  KuDtoaBig left = ku_dtoa_big_u64(mantissa);
  KuDtoaBig right = ku_dtoa_big_u64(UINT64_C(1));
  bool ok = binary_exponent >= 0
      ? ku_dtoa_big_mul_pow2(&left, binary_exponent)
      : ku_dtoa_big_mul_pow2(&right, -binary_exponent);
  if (!ok) return false;
  ok = decimal_exponent >= 0
      ? ku_dtoa_big_mul_pow10(&right, decimal_exponent)
      : ku_dtoa_big_mul_pow10(&left, -decimal_exponent);
  if (!ok) return false;
  *comparison = ku_dtoa_big_compare(&left, &right);
  return true;
}

/* Compare decimal_mantissa * 10^decimal_exponent with
 * binary_mantissa * 2^binary_exponent. */
static bool ku_dtoa_compare_decimal_binary(
    uint64_t decimal_mantissa,
    int decimal_exponent,
    uint64_t binary_mantissa,
    int binary_exponent,
    int* comparison) {
  KuDtoaBig left = ku_dtoa_big_u64(decimal_mantissa);
  KuDtoaBig right = ku_dtoa_big_u64(binary_mantissa);
  bool ok = decimal_exponent >= 0
      ? ku_dtoa_big_mul_pow10(&left, decimal_exponent)
      : ku_dtoa_big_mul_pow10(&right, -decimal_exponent);
  if (!ok) return false;
  ok = binary_exponent >= 0
      ? ku_dtoa_big_mul_pow2(&right, binary_exponent)
      : ku_dtoa_big_mul_pow2(&left, -binary_exponent);
  if (!ok) return false;
  *comparison = ku_dtoa_big_compare(&left, &right);
  return true;
}

static bool ku_dtoa_ratio(
    uint64_t mantissa,
    int binary_exponent,
    int decimal_exponent,
    KuDtoaBig* numerator,
    KuDtoaBig* denominator) {
  *numerator = ku_dtoa_big_u64(mantissa);
  *denominator = ku_dtoa_big_u64(UINT64_C(1));
  bool ok = binary_exponent >= 0
      ? ku_dtoa_big_mul_pow2(numerator, binary_exponent)
      : ku_dtoa_big_mul_pow2(denominator, -binary_exponent);
  if (!ok) return false;
  return decimal_exponent >= 0
      ? ku_dtoa_big_mul_pow10(denominator, decimal_exponent)
      : ku_dtoa_big_mul_pow10(numerator, -decimal_exponent);
}

static bool ku_dtoa_floor_ratio(
    const KuDtoaBig* numerator,
    const KuDtoaBig* denominator,
    uint64_t exclusive_limit,
    uint64_t* quotient) {
  uint64_t low = 0;
  uint64_t high = exclusive_limit;
  while (high - low > 1) { /* at most 57 steps for 10^17 */
    uint64_t middle = low + (high - low) / 2;
    KuDtoaBig product;
    if (!ku_dtoa_big_mul_u64(denominator, middle, &product)) return false;
    if (ku_dtoa_big_compare(&product, numerator) <= 0) low = middle;
    else high = middle;
  }
  *quotient = low;
  return true;
}

static bool ku_dtoa_candidate_in_interval(
    uint64_t decimal_mantissa,
    int decimal_exponent,
    uint64_t binary_mantissa,
    int binary_exponent,
    uint16_t raw_exponent,
    uint64_t raw_fraction,
    bool* in_interval) {
  uint64_t lower_mantissa;
  uint64_t upper_mantissa;
  int boundary_exponent;
  if (raw_exponent > 1 && raw_fraction == 0) {
    lower_mantissa = binary_mantissa * UINT64_C(4) - UINT64_C(1);
    upper_mantissa = binary_mantissa * UINT64_C(4) + UINT64_C(2);
    boundary_exponent = binary_exponent - 2;
  } else {
    lower_mantissa = binary_mantissa * UINT64_C(2) - UINT64_C(1);
    upper_mantissa = binary_mantissa * UINT64_C(2) + UINT64_C(1);
    boundary_exponent = binary_exponent - 1;
  }
  int lower_comparison = 0;
  int upper_comparison = 0;
  if (!ku_dtoa_compare_decimal_binary(decimal_mantissa, decimal_exponent,
          lower_mantissa, boundary_exponent, &lower_comparison)
      || !ku_dtoa_compare_decimal_binary(decimal_mantissa, decimal_exponent,
          upper_mantissa, boundary_exponent, &upper_comparison)) return false;
  bool accepts_boundary = (binary_mantissa & UINT64_C(1)) == 0;
  bool above_lower = lower_comparison > 0
      || (lower_comparison == 0 && accepts_boundary);
  bool below_upper = upper_comparison < 0
      || (upper_comparison == 0 && accepts_boundary);
  *in_interval = above_lower && below_upper;
  return true;
}

static bool ku_dtoa_format_finite(
    double value,
    char* output,
    size_t capacity,
    size_t* output_len) {
  static const uint64_t powers_of_ten[18] = {
    UINT64_C(1), UINT64_C(10), UINT64_C(100), UINT64_C(1000),
    UINT64_C(10000), UINT64_C(100000), UINT64_C(1000000),
    UINT64_C(10000000), UINT64_C(100000000), UINT64_C(1000000000),
    UINT64_C(10000000000), UINT64_C(100000000000),
    UINT64_C(1000000000000), UINT64_C(10000000000000),
    UINT64_C(100000000000000), UINT64_C(1000000000000000),
    UINT64_C(10000000000000000), UINT64_C(100000000000000000)
  };
  uint64_t bits = 0;
  if (sizeof(value) != sizeof(bits)) return false;
  memcpy(&bits, &value, sizeof(bits));
  bool negative = (bits >> 63) != 0;
  uint16_t raw_exponent = (uint16_t)((bits >> 52) & UINT64_C(0x7ff));
  uint64_t raw_fraction = bits & UINT64_C(0x000fffffffffffff);
  if (raw_exponent == 0x7ff) return false;
  if (raw_exponent == 0 && raw_fraction == 0) {
    size_t needed = negative ? 2 : 1;
    if (capacity < needed) return false;
    size_t at = 0;
    if (negative) output[at++] = '-';
    output[at++] = '0';
    *output_len = at;
    return true;
  }

  uint64_t binary_mantissa;
  int binary_exponent;
  int floor_binary_exponent;
  if (raw_exponent == 0) {
    binary_mantissa = raw_fraction;
    binary_exponent = -1074;
    int highest = 51;
    while (highest > 0
        && (binary_mantissa & (UINT64_C(1) << (unsigned)highest)) == 0) highest--;
    floor_binary_exponent = highest - 1074;
  } else {
    binary_mantissa = (UINT64_C(1) << 52) | raw_fraction;
    binary_exponent = (int)raw_exponent - 1023 - 52;
    floor_binary_exponent = (int)raw_exponent - 1023;
  }

  /* floor(log10(value)); fixed-point log10(2) estimate, then exact correction. */
  int64_t scaled = (int64_t)floor_binary_exponent * INT64_C(78913);
  int decimal_magnitude = (int)(scaled / INT64_C(262144));
  if (scaled < 0 && scaled % INT64_C(262144) != 0) decimal_magnitude--;
  bool magnitude_done = false;
  for (int correction = 0; correction < 4; correction++) {
    int comparison = 0;
    if (!ku_dtoa_compare_value_pow10(binary_mantissa, binary_exponent,
            decimal_magnitude, &comparison)) return false;
    if (comparison < 0) {
      decimal_magnitude--;
      continue;
    }
    if (!ku_dtoa_compare_value_pow10(binary_mantissa, binary_exponent,
            decimal_magnitude + 1, &comparison)) return false;
    if (comparison >= 0) {
      decimal_magnitude++;
      continue;
    }
    magnitude_done = true;
    break;
  }
  if (!magnitude_done) return false;

  uint64_t shortest = 0;
  int shortest_exponent = 0;
  bool found = false;
  for (int digits = 1; digits <= 17; digits++) {
    int decimal_exponent = decimal_magnitude - digits + 1;
    KuDtoaBig numerator;
    KuDtoaBig denominator;
    if (!ku_dtoa_ratio(binary_mantissa, binary_exponent, decimal_exponent,
            &numerator, &denominator)) return false;
    uint64_t floor_value = 0;
    if (!ku_dtoa_floor_ratio(&numerator, &denominator,
            powers_of_ten[digits], &floor_value)) return false;
    KuDtoaBig floor_product;
    if (!ku_dtoa_big_mul_u64(&denominator, floor_value, &floor_product)) return false;
    bool exact = ku_dtoa_big_compare(&floor_product, &numerator) == 0;
    uint64_t ceil_value = exact ? floor_value : floor_value + UINT64_C(1);
    bool floor_valid = false;
    bool ceil_valid = false;
    if (!ku_dtoa_candidate_in_interval(floor_value, decimal_exponent,
            binary_mantissa, binary_exponent, raw_exponent, raw_fraction,
            &floor_valid)) return false;
    if (exact) {
      ceil_valid = floor_valid;
    } else if (!ku_dtoa_candidate_in_interval(ceil_value, decimal_exponent,
                   binary_mantissa, binary_exponent, raw_exponent, raw_fraction,
                   &ceil_valid)) return false;
    if (!floor_valid && !ceil_valid) continue;

    uint64_t selected;
    if (floor_valid && !ceil_valid) selected = floor_value;
    else if (!floor_valid && ceil_valid) selected = ceil_value;
    else if (exact) selected = floor_value;
    else {
      KuDtoaBig twice_numerator = numerator;
      if (!ku_dtoa_big_mul_small(&twice_numerator, UINT32_C(2))) return false;
      KuDtoaBig midpoint;
      if (!ku_dtoa_big_mul_u64(&denominator,
              floor_value * UINT64_C(2) + UINT64_C(1), &midpoint)) return false;
      int midpoint_comparison = ku_dtoa_big_compare(&twice_numerator, &midpoint);
      if (midpoint_comparison < 0) selected = floor_value;
      else if (midpoint_comparison > 0) selected = ceil_value;
      else selected = ceil_value; /* Rust shortest-mode midpoint rule. */
    }
    shortest = selected;
    shortest_exponent = decimal_exponent;
    while (shortest != 0 && shortest % UINT64_C(10) == 0) {
      shortest /= UINT64_C(10);
      shortest_exponent++;
    }
    found = true;
    break;
  }
  if (!found || shortest == 0) return false;

  char reversed[18];
  size_t digit_count = 0;
  while (shortest != 0) {
    reversed[digit_count++] = (char)('0' + shortest % UINT64_C(10));
    shortest /= UINT64_C(10);
  }
  char digits[18];
  for (size_t index = 0; index < digit_count; index++) {
    digits[index] = reversed[digit_count - index - 1];
  }
  int decimal_point = (int)digit_count + shortest_exponent;
  size_t needed = negative ? 1 : 0;
  if (decimal_point <= 0) needed += 2 + (size_t)(-decimal_point) + digit_count;
  else if ((size_t)decimal_point >= digit_count) needed += (size_t)decimal_point;
  else needed += digit_count + 1;
  if (needed > capacity || needed >= KU_DTOA_TEXT_CAP) return false;

  size_t at = 0;
  if (negative) output[at++] = '-';
  if (decimal_point <= 0) {
    output[at++] = '0';
    output[at++] = '.';
    for (int zero = 0; zero < -decimal_point; zero++) output[at++] = '0';
    memcpy(output + at, digits, digit_count);
    at += digit_count;
  } else if ((size_t)decimal_point >= digit_count) {
    memcpy(output + at, digits, digit_count);
    at += digit_count;
    while (at < needed) output[at++] = '0';
  } else {
    memcpy(output + at, digits, (size_t)decimal_point);
    at += (size_t)decimal_point;
    output[at++] = '.';
    memcpy(output + at, digits + decimal_point, digit_count - (size_t)decimal_point);
    at += digit_count - (size_t)decimal_point;
  }
  *output_len = at;
  return true;
}

static KuError ku_json_error(const char* code, const char* message) {
  return ku_error_make(
      ku_string_static((const uint8_t*)"json", 4),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}

static bool ku_json_fail(KuError* error, const char* code, const char* message) {
  if (error && !error->code.ptr) *error = ku_json_error(code, message);
  return false;
}

static bool ku_json_write_unsupported(
    size_t depth, KuError* error, const char* message) {
  if (depth > KU_JSON_MAX_DEPTH) {
    return ku_json_fail(error, "stringify_error", "json value nesting is too deep");
  }
  return ku_json_fail(error, "stringify_error", message);
}

static void ku_json_buffer_drop(KuJsonBuffer* buffer) {
  if (!buffer) return;
  free(buffer->data);
  *buffer = (KuJsonBuffer){0};
}

static bool ku_json_buffer_append(
    KuJsonBuffer* buffer,
    const uint8_t* bytes,
    size_t len,
    size_t limit,
    KuError* error,
    const char* limit_message) {
  if (buffer->len > limit || len > limit - buffer->len) {
    return ku_json_fail(error, "stringify_error", limit_message);
  }
  size_t needed = buffer->len + len;
  if (needed > buffer->cap) {
    size_t next = buffer->cap ? buffer->cap : 64;
    while (next < needed) {
      size_t grown = next > limit / 2 ? limit : next * 2;
      if (grown <= next) {
        return ku_json_fail(error, "stringify_error", limit_message);
      }
      next = grown;
    }
    uint8_t* data = (uint8_t*)realloc(buffer->data, next ? next : 1);
    if (!data) return ku_json_fail(error, "out_of_memory", "json allocation failed");
    buffer->data = data;
    buffer->cap = next;
  }
  if (len) memcpy(buffer->data + buffer->len, bytes, len);
  buffer->len = needed;
  return true;
}

static bool ku_json_buffer_byte(
    KuJsonBuffer* buffer,
    uint8_t byte,
    size_t limit,
    KuError* error,
    const char* limit_message) {
  return ku_json_buffer_append(buffer, &byte, 1, limit, error, limit_message);
}

static KuString ku_json_buffer_take_string(KuJsonBuffer* buffer) {
  if (!buffer->data) return ku_string_static((const uint8_t*)"", 0);
  KuString value = (KuString){ buffer->data, buffer->len, buffer->cap, KU_STRING_OWNED };
  *buffer = (KuJsonBuffer){0};
  return value;
}

static bool ku_json_utf8_scalar(
    const uint8_t* current,
    const uint8_t* end,
    size_t* width,
    uint32_t* scalar) {
  if (!current || current >= end) return false;
  uint8_t first = current[0];
  if (first < 0x80) {
    *width = 1;
    *scalar = first;
    return true;
  }
  size_t remaining = (size_t)(end - current);
  if (first >= 0xC2 && first <= 0xDF) {
    if (remaining < 2 || (current[1] & 0xC0) != 0x80) return false;
    *width = 2;
    *scalar = ((uint32_t)(first & 0x1F) << 6) | (uint32_t)(current[1] & 0x3F);
    return true;
  }
  if (first >= 0xE0 && first <= 0xEF) {
    if (remaining < 3 || (current[1] & 0xC0) != 0x80 || (current[2] & 0xC0) != 0x80) return false;
    if (first == 0xE0 && current[1] < 0xA0) return false;
    if (first == 0xED && current[1] >= 0xA0) return false;
    *width = 3;
    *scalar = ((uint32_t)(first & 0x0F) << 12)
        | ((uint32_t)(current[1] & 0x3F) << 6)
        | (uint32_t)(current[2] & 0x3F);
    return true;
  }
  if (first >= 0xF0 && first <= 0xF4) {
    if (remaining < 4 || (current[1] & 0xC0) != 0x80
        || (current[2] & 0xC0) != 0x80 || (current[3] & 0xC0) != 0x80) return false;
    if (first == 0xF0 && current[1] < 0x90) return false;
    if (first == 0xF4 && current[1] > 0x8F) return false;
    *width = 4;
    *scalar = ((uint32_t)(first & 0x07) << 18)
        | ((uint32_t)(current[1] & 0x3F) << 12)
        | ((uint32_t)(current[2] & 0x3F) << 6)
        | (uint32_t)(current[3] & 0x3F);
    return *scalar <= 0x10FFFF;
  }
  return false;
}

static size_t ku_json_encode_utf8(uint32_t scalar, uint8_t bytes[4]) {
  if (scalar <= 0x7F) {
    bytes[0] = (uint8_t)scalar;
    return 1;
  }
  if (scalar <= 0x7FF) {
    bytes[0] = (uint8_t)(0xC0 | (scalar >> 6));
    bytes[1] = (uint8_t)(0x80 | (scalar & 0x3F));
    return 2;
  }
  if (scalar <= 0xFFFF) {
    bytes[0] = (uint8_t)(0xE0 | (scalar >> 12));
    bytes[1] = (uint8_t)(0x80 | ((scalar >> 6) & 0x3F));
    bytes[2] = (uint8_t)(0x80 | (scalar & 0x3F));
    return 3;
  }
  bytes[0] = (uint8_t)(0xF0 | (scalar >> 18));
  bytes[1] = (uint8_t)(0x80 | ((scalar >> 12) & 0x3F));
  bytes[2] = (uint8_t)(0x80 | ((scalar >> 6) & 0x3F));
  bytes[3] = (uint8_t)(0x80 | (scalar & 0x3F));
  return 4;
}

static void ku_json_skip_ws(KuJsonParser* parser) {
  while (parser->current < parser->end) {
    uint8_t byte = *parser->current;
    if (byte != ' ' && byte != '\t' && byte != '\n' && byte != '\r') break;
    parser->current++;
  }
}

static bool ku_json_parse_value(
    KuJsonParser* parser,
    size_t depth,
    KuValue* value,
    KuError* error);

static bool ku_json_parse_string(
    KuJsonParser* parser,
    KuString* value,
    KuError* error) {
  *value = (KuString){0};
  if (parser->current >= parser->end || *parser->current != '"') {
    return ku_json_fail(error, "parse_error", "expected json string");
  }
  parser->current++;
  KuJsonBuffer buffer = {0};
  while (parser->current < parser->end) {
    uint8_t byte = *parser->current++;
    if (byte == '"') {
      *value = ku_json_buffer_take_string(&buffer);
      return true;
    }
    if (byte == '\\') {
      if (parser->current >= parser->end) {
        ku_json_buffer_drop(&buffer);
        return ku_json_fail(error, "parse_error", "unterminated json escape");
      }
      uint8_t escaped = *parser->current++;
      uint8_t decoded = 0;
      bool one_byte = true;
      switch (escaped) {
        case '"': decoded = '"'; break;
        case '\\': decoded = '\\'; break;
        case '/': decoded = '/'; break;
        case 'b': decoded = 8; break;
        case 'f': decoded = 12; break;
        case 'n': decoded = '\n'; break;
        case 'r': decoded = '\r'; break;
        case 't': decoded = '\t'; break;
        case 'u': {
          one_byte = false;
          uint32_t scalar = 0;
          for (size_t index = 0; index < 4; index++) {
            if (parser->current >= parser->end) {
              ku_json_buffer_drop(&buffer);
              return ku_json_fail(error, "parse_error", "unterminated unicode escape");
            }
            uint8_t hex = *parser->current++;
            uint32_t digit;
            if (hex >= '0' && hex <= '9') digit = (uint32_t)(hex - '0');
            else if (hex >= 'a' && hex <= 'f') digit = (uint32_t)(hex - 'a' + 10);
            else if (hex >= 'A' && hex <= 'F') digit = (uint32_t)(hex - 'A' + 10);
            else {
              ku_json_buffer_drop(&buffer);
              return ku_json_fail(error, "parse_error", "invalid unicode escape");
            }
            scalar = scalar * 16 + digit;
          }
          if (scalar >= 0xD800 && scalar <= 0xDBFF) {
            if ((size_t)(parser->end - parser->current) < 6
                || parser->current[0] != '\\' || parser->current[1] != 'u') {
              ku_json_buffer_drop(&buffer);
              return ku_json_fail(error, "parse_error",
                  "high surrogate must be followed by a low surrogate");
            }
            parser->current += 2;
            uint32_t low = 0;
            for (size_t index = 0; index < 4; index++) {
              uint8_t hex = *parser->current++;
              uint32_t digit;
              if (hex >= '0' && hex <= '9') digit = (uint32_t)(hex - '0');
              else if (hex >= 'a' && hex <= 'f') digit = (uint32_t)(hex - 'a' + 10);
              else if (hex >= 'A' && hex <= 'F') digit = (uint32_t)(hex - 'A' + 10);
              else {
                ku_json_buffer_drop(&buffer);
                return ku_json_fail(error, "parse_error", "invalid unicode escape");
              }
              low = low * 16 + digit;
            }
            if (low < 0xDC00 || low > 0xDFFF) {
              ku_json_buffer_drop(&buffer);
              return ku_json_fail(error, "parse_error",
                  "high surrogate must be followed by a low surrogate");
            }
            scalar = 0x10000 + ((scalar - 0xD800) << 10) + (low - 0xDC00);
          } else if (scalar >= 0xDC00 && scalar <= 0xDFFF) {
            ku_json_buffer_drop(&buffer);
            return ku_json_fail(error, "parse_error", "unexpected low surrogate");
          }
          uint8_t encoded[4];
          size_t encoded_len = ku_json_encode_utf8(scalar, encoded);
          if (!ku_json_buffer_append(&buffer, encoded, encoded_len,
                  KU_JSON_MAX_INPUT_BYTES, error, "json string is too large")) {
            ku_json_buffer_drop(&buffer);
            return false;
          }
          break;
        }
        default:
          ku_json_buffer_drop(&buffer);
          return ku_json_fail(error, "parse_error", "invalid json escape");
      }
      if (one_byte && !ku_json_buffer_byte(&buffer, decoded,
              KU_JSON_MAX_INPUT_BYTES, error, "json string is too large")) {
        ku_json_buffer_drop(&buffer);
        return false;
      }
      continue;
    }
    if (byte < 0x20) {
      ku_json_buffer_drop(&buffer);
      return ku_json_fail(error, "parse_error", "control character in json string");
    }
    if (byte < 0x80) {
      if (!ku_json_buffer_byte(&buffer, byte, KU_JSON_MAX_INPUT_BYTES,
              error, "json string is too large")) {
        ku_json_buffer_drop(&buffer);
        return false;
      }
      continue;
    }
    parser->current--;
    size_t width = 0;
    uint32_t scalar = 0;
    if (!ku_json_utf8_scalar(parser->current, parser->end, &width, &scalar)) {
      ku_json_buffer_drop(&buffer);
      return ku_json_fail(error, "parse_error", "invalid utf-8 in json string");
    }
    if (!ku_json_buffer_append(&buffer, parser->current, width,
            KU_JSON_MAX_INPUT_BYTES, error, "json string is too large")) {
      ku_json_buffer_drop(&buffer);
      return false;
    }
    parser->current += width;
  }
  ku_json_buffer_drop(&buffer);
  return ku_json_fail(error, "parse_error", "unterminated json string");
}

static bool ku_json_parse_literal(
    KuJsonParser* parser,
    const char* literal,
    KuValue result,
    KuValue* value,
    KuError* error) {
  size_t len = strlen(literal);
  if ((size_t)(parser->end - parser->current) < len
      || memcmp(parser->current, literal, len) != 0) {
    return ku_json_fail(error, "parse_error", "invalid json literal");
  }
  parser->current += len;
  *value = result;
  return true;
}

static bool ku_json_parse_number(
    KuJsonParser* parser,
    KuValue* value,
    KuError* error) {
  const uint8_t* start = parser->current;
  bool negative = false;
  if (parser->current < parser->end && *parser->current == '-') {
    negative = true;
    parser->current++;
  }
  if (parser->current >= parser->end) {
    return ku_json_fail(error, "parse_error", "expected digit in json number");
  }
  if (*parser->current == '0') {
    parser->current++;
    if (parser->current < parser->end
        && *parser->current >= '0' && *parser->current <= '9') {
      return ku_json_fail(error, "parse_error", "leading zero in json number");
    }
  } else if (*parser->current >= '1' && *parser->current <= '9') {
    while (parser->current < parser->end
        && *parser->current >= '0' && *parser->current <= '9') parser->current++;
  } else {
    return ku_json_fail(error, "parse_error", "expected digit in json number");
  }

  bool is_float = false;
  if (parser->current < parser->end && *parser->current == '.') {
    is_float = true;
    parser->current++;
    const uint8_t* digits = parser->current;
    while (parser->current < parser->end
        && *parser->current >= '0' && *parser->current <= '9') parser->current++;
    if (parser->current == digits) {
      return ku_json_fail(error, "parse_error", "expected digit after decimal point");
    }
  }
  if (parser->current < parser->end
      && (*parser->current == 'e' || *parser->current == 'E')) {
    is_float = true;
    parser->current++;
    if (parser->current < parser->end
        && (*parser->current == '+' || *parser->current == '-')) parser->current++;
    const uint8_t* digits = parser->current;
    while (parser->current < parser->end
        && *parser->current >= '0' && *parser->current <= '9') parser->current++;
    if (parser->current == digits) {
      return ku_json_fail(error, "parse_error", "expected digit in exponent");
    }
  }

  size_t len = (size_t)(parser->current - start);
  if (is_float) {
    if (len == SIZE_MAX) return ku_json_fail(error, "parse_error", "invalid json number");
    char* text = (char*)malloc(len + 1);
    if (!text) return ku_json_fail(error, "out_of_memory", "json allocation failed");
    memcpy(text, start, len);
    text[len] = '\0';
    char* parsed_end = NULL;
    errno = 0;
    double number = strtod(text, &parsed_end);
    bool valid = parsed_end == text + len && isfinite(number);
    free(text);
    if (!valid) return ku_json_fail(error, "parse_error", "json number must be finite");
    *value = ku_v_float(number);
    return true;
  }

  const uint8_t* digit = start + (negative ? 1 : 0);
  uint64_t magnitude = 0;
  uint64_t limit = negative ? ((uint64_t)INT64_MAX + 1u) : (uint64_t)INT64_MAX;
  while (digit < parser->current) {
    uint64_t next = (uint64_t)(*digit - '0');
    if (magnitude > (limit - next) / 10u) {
      return ku_json_fail(error, "parse_error", "invalid json number");
    }
    magnitude = magnitude * 10u + next;
    digit++;
  }
  int64_t number;
  if (negative && magnitude == ((uint64_t)INT64_MAX + 1u)) number = INT64_MIN;
  else if (negative) number = -(int64_t)magnitude;
  else number = (int64_t)magnitude;
  *value = ku_v_int(number);
  return true;
}

static bool ku_json_parse_array(
    KuJsonParser* parser,
    size_t depth,
    KuValue* value,
    KuError* error) {
  parser->current++;
  KuValueArray* array = NULL;
  if (!ku_value_array_try_new(0, &array)) {
    return ku_json_fail(error, "out_of_memory", "json allocation failed");
  }
  ku_json_skip_ws(parser);
  if (parser->current < parser->end && *parser->current == ']') {
    parser->current++;
    *value = ku_v_array(array);
    return true;
  }
  while (true) {
    KuValue element = ku_v_null();
    if (!ku_json_parse_value(parser, depth, &element, error)) {
      ku_value_drop(&element);
      ku_value_array_drop(array);
      return false;
    }
    if (!ku_value_array_try_push_owned(array, &element)) {
      ku_value_drop(&element);
      ku_value_array_drop(array);
      return ku_json_fail(error, "out_of_memory", "json allocation failed");
    }
    ku_json_skip_ws(parser);
    if (parser->current < parser->end && *parser->current == ']') {
      parser->current++;
      *value = ku_v_array(array);
      return true;
    }
    if (parser->current >= parser->end || *parser->current != ',') {
      ku_value_array_drop(array);
      return ku_json_fail(error, "parse_error", "expected ','");
    }
    parser->current++;
  }
}

static bool ku_json_parse_object(
    KuJsonParser* parser,
    size_t depth,
    KuValue* value,
    KuError* error) {
  parser->current++;
  KuObject* object = NULL;
  if (!ku_object_try_new(0, &object)) {
    return ku_json_fail(error, "out_of_memory", "json allocation failed");
  }
  ku_json_skip_ws(parser);
  if (parser->current < parser->end && *parser->current == '}') {
    parser->current++;
    *value = ku_v_object(object);
    return true;
  }
  while (true) {
    ku_json_skip_ws(parser);
    KuString key = (KuString){0};
    if (!ku_json_parse_string(parser, &key, error)) {
      ku_object_drop(object);
      return false;
    }
    ku_json_skip_ws(parser);
    if (parser->current >= parser->end || *parser->current != ':') {
      ku_string_drop(&key);
      ku_object_drop(object);
      return ku_json_fail(error, "parse_error", "expected ':'");
    }
    parser->current++;
    KuValue field = ku_v_null();
    if (!ku_json_parse_value(parser, depth, &field, error)) {
      ku_string_drop(&key);
      ku_value_drop(&field);
      ku_object_drop(object);
      return false;
    }
    if (!ku_object_try_set_owned(object, &key, &field)) {
      ku_string_drop(&key);
      ku_value_drop(&field);
      ku_object_drop(object);
      return ku_json_fail(error, "out_of_memory", "json allocation failed");
    }
    ku_json_skip_ws(parser);
    if (parser->current < parser->end && *parser->current == '}') {
      parser->current++;
      *value = ku_v_object(object);
      return true;
    }
    if (parser->current >= parser->end || *parser->current != ',') {
      ku_object_drop(object);
      return ku_json_fail(error, "parse_error", "expected ','");
    }
    parser->current++;
  }
}

static bool ku_json_parse_value(
    KuJsonParser* parser,
    size_t depth,
    KuValue* value,
    KuError* error) {
  *value = ku_v_null();
  if (depth > KU_JSON_MAX_DEPTH) {
    return ku_json_fail(error, "parse_error", "json nesting is too deep");
  }
  ku_json_skip_ws(parser);
  if (parser->current >= parser->end) {
    return ku_json_fail(error, "parse_error", "unexpected end of json");
  }
  switch (*parser->current) {
    case '"': {
      KuString string = (KuString){0};
      if (!ku_json_parse_string(parser, &string, error)) return false;
      *value = ku_v_str(string);
      return true;
    }
    case 'n': return ku_json_parse_literal(parser, "null", ku_v_null(), value, error);
    case 't': return ku_json_parse_literal(parser, "true", ku_v_bool(true), value, error);
    case 'f': return ku_json_parse_literal(parser, "false", ku_v_bool(false), value, error);
    case '[': return ku_json_parse_array(parser, depth + 1, value, error);
    case '{': return ku_json_parse_object(parser, depth + 1, value, error);
    default:
      if (*parser->current == '-'
          || (*parser->current >= '0' && *parser->current <= '9')) {
        return ku_json_parse_number(parser, value, error);
      }
      return ku_json_fail(error, "parse_error", "expected json value");
  }
}

static KuResult_kuvalue ku_json_try_parse(KuString text) {
  if (text.len > KU_JSON_MAX_INPUT_BYTES) {
    return (KuResult_kuvalue){ false, ku_v_null(),
        ku_json_error("parse_error", "json input is too large") };
  }
  if (text.len && !text.ptr) {
    return (KuResult_kuvalue){ false, ku_v_null(),
        ku_json_error("parse_error", "invalid utf-8 in json input") };
  }
  const uint8_t* begin = text.ptr ? text.ptr : (const uint8_t*)"";
  KuJsonParser parser = { begin, begin + text.len };
  KuValue value = ku_v_null();
  KuError error = (KuError){0};
  if (!ku_json_parse_value(&parser, 0, &value, &error)) {
    ku_value_drop(&value);
    if (!error.code.ptr) error = ku_json_error("parse_error", "invalid json");
    return (KuResult_kuvalue){ false, ku_v_null(), error };
  }
  ku_json_skip_ws(&parser);
  if (parser.current != parser.end) {
    ku_value_drop(&value);
    return (KuResult_kuvalue){ false, ku_v_null(),
        ku_json_error("parse_error", "unexpected trailing characters") };
  }
  return (KuResult_kuvalue){ true, value, (KuError){0} };
}

static KuResult_kuvalue ku_json_parse(KuString text) {
  return ku_json_try_parse(text);
}

static int ku_json_entry_compare(const void* left_ptr, const void* right_ptr) {
  const KuEntry* left = *(const KuEntry* const*)left_ptr;
  const KuEntry* right = *(const KuEntry* const*)right_ptr;
  size_t shared = left->key.len < right->key.len ? left->key.len : right->key.len;
  int order = shared ? memcmp(left->key.ptr, right->key.ptr, shared) : 0;
  if (order != 0) return order;
  if (left->key.len < right->key.len) return -1;
  if (left->key.len > right->key.len) return 1;
  return 0;
}

static bool ku_json_write_value(
    KuJsonBuffer* output,
    KuValue value,
    size_t depth,
    KuError* error);

static bool ku_json_write_bytes(
    KuJsonBuffer* output,
    const char* bytes,
    size_t len,
    KuError* error) {
  return ku_json_buffer_append(output, (const uint8_t*)bytes, len,
      KU_JSON_MAX_OUTPUT_BYTES, error, "json.stringify output is too large");
}

static bool ku_json_write_byte(
    KuJsonBuffer* output,
    uint8_t byte,
    KuError* error) {
  return ku_json_buffer_byte(output, byte, KU_JSON_MAX_OUTPUT_BYTES,
      error, "json.stringify output is too large");
}

static bool ku_json_write_string(
    KuJsonBuffer* output,
    KuString string,
    KuError* error) {
  if (string.len && !string.ptr) {
    return ku_json_fail(error, "stringify_error", "invalid utf-8 in json string");
  }
  if (!ku_json_write_byte(output, '"', error)) return false;
  const uint8_t* current = string.ptr ? string.ptr : (const uint8_t*)"";
  const uint8_t* end = current + string.len;
  while (current < end) {
    size_t width = 0;
    uint32_t scalar = 0;
    if (!ku_json_utf8_scalar(current, end, &width, &scalar)) {
      return ku_json_fail(error, "stringify_error", "invalid utf-8 in json string");
    }
    if (scalar == '"' || scalar == '\\') {
      uint8_t escaped[2] = { '\\', (uint8_t)scalar };
      if (!ku_json_buffer_append(output, escaped, 2, KU_JSON_MAX_OUTPUT_BYTES,
              error, "json.stringify output is too large")) return false;
    } else if (scalar == '\n' || scalar == '\r' || scalar == '\t') {
      uint8_t escaped[2] = { '\\', scalar == '\n' ? 'n' : (scalar == '\r' ? 'r' : 't') };
      if (!ku_json_buffer_append(output, escaped, 2, KU_JSON_MAX_OUTPUT_BYTES,
              error, "json.stringify output is too large")) return false;
    } else if (scalar < 0x20) {
      char escaped[7];
      int len = snprintf(escaped, sizeof(escaped), "\\u%04x", (unsigned)scalar);
      if (len != 6 || !ku_json_write_bytes(output, escaped, 6, error)) return false;
    } else if (!ku_json_buffer_append(output, current, width, KU_JSON_MAX_OUTPUT_BYTES,
                   error, "json.stringify output is too large")) {
      return false;
    }
    current += width;
  }
  return ku_json_write_byte(output, '"', error);
}

static bool ku_json_write_object(
    KuJsonBuffer* output,
    KuObject* object,
    size_t depth,
    KuError* error) {
  if (!ku_json_write_byte(output, '{', error)) return false;
  size_t count = object ? object->len : 0;
  if (count == 0) return ku_json_write_byte(output, '}', error);
  if (count > SIZE_MAX / sizeof(KuEntry*)) {
    return ku_json_fail(error, "stringify_error", "json object is too large");
  }
  KuEntry** entries = (KuEntry**)malloc(count * sizeof(KuEntry*));
  if (!entries) return ku_json_fail(error, "out_of_memory", "json allocation failed");
  size_t found = 0;
  for (size_t index = 0; index < object->cap && found < count; index++) {
    if (object->entries[index].used) entries[found++] = &object->entries[index];
  }
  qsort(entries, found, sizeof(KuEntry*), ku_json_entry_compare);
  for (size_t index = 0; index < found; index++) {
    bool ok = (index == 0 || ku_json_write_byte(output, ',', error))
        && ku_json_write_string(output, entries[index]->key, error)
        && ku_json_write_byte(output, ':', error)
        && ku_json_write_value(output, entries[index]->value, depth + 1, error);
    if (!ok) {
      free(entries);
      return false;
    }
  }
  free(entries);
  return ku_json_write_byte(output, '}', error);
}

static bool ku_json_write_value(
    KuJsonBuffer* output,
    KuValue value,
    size_t depth,
    KuError* error) {
  if (depth > KU_JSON_MAX_DEPTH) {
    return ku_json_fail(error, "stringify_error", "json value nesting is too deep");
  }
  char number[KU_DTOA_TEXT_CAP];
  switch (value.tag) {
    case KU_NULL:
      return ku_json_write_bytes(output, "null", 4, error);
    case KU_INT: {
      int len = snprintf(number, sizeof(number), "%lld", (long long)value.as.i);
      return len > 0 && (size_t)len < sizeof(number)
          && ku_json_write_bytes(output, number, (size_t)len, error);
    }
    case KU_FLOAT: {
      if (!isfinite(value.as.f)) {
        return ku_json_fail(error, "stringify_error",
            "json.stringify does not support non-finite float");
      }
      size_t len = 0;
      if (!ku_dtoa_format_finite(value.as.f, number, sizeof(number), &len)) {
        return ku_json_fail(error, "stringify_error",
            "json float formatting failed");
      }
      return ku_json_write_bytes(output, number, len, error);
    }
    case KU_BOOL:
      return value.as.b
          ? ku_json_write_bytes(output, "true", 4, error)
          : ku_json_write_bytes(output, "false", 5, error);
    case KU_STR:
      return ku_json_write_string(output, value.as.s, error);
    case KU_OBJECT:
      return ku_json_write_object(output, value.as.o, depth, error);
    case KU_ARRAY: {
      if (!ku_json_write_byte(output, '[', error)) return false;
      KuValueArray* array = value.as.arr;
      if (array) {
        for (size_t index = 0; index < array->len; index++) {
          if ((index != 0 && !ku_json_write_byte(output, ',', error))
              || !ku_json_write_value(output, array->data[index], depth + 1, error)) {
            return false;
          }
        }
      }
      return ku_json_write_byte(output, ']', error);
    }
    case KU_FUNCTION:
      return ku_json_fail(error, "stringify_error",
          "json.stringify does not support function");
    default:
      return ku_json_fail(error, "stringify_error",
          "json.stringify does not support value");
  }
}

static KuResult_str ku_json_stringify(KuValue value) {
  KuJsonBuffer output = {0};
  KuError error = (KuError){0};
  bool ok = ku_json_write_value(&output, value, 0, &error);
  if (!ok) {
    ku_json_buffer_drop(&output);
    if (!error.code.ptr) error = ku_json_error("stringify_error", "json stringify failed");
    return (KuResult_str){ false, (KuString){0}, error };
  }
  ku_error_drop(&error);
  return (KuResult_str){ true, ku_json_buffer_take_string(&output), (KuError){0} };
}

"#,
    );
}

/// Emit a dynamic object literal as statements building a KuObject*. Each field
/// is materialized into owned key/value locals before the fallible insertion;
/// allocation failure therefore drops the uncommitted field and every field
/// already committed to the partial object before the legacy hard-fail adapter.
fn try_emit_object_construction(out: &mut COutput, target: &str, value: &IrExpr) -> KuResult<bool> {
    out.check()?;
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
    out.push_str(&format!(
        "  if (!ku_object_try_new(0, &{target})) ku_object_hard_fail_oom();\n"
    ));
    let mut i = 0;
    while i + 1 < args.len() {
        let key = &args[i];
        let field = &args[i + 1];
        let field_index = i / 2;
        let key_local = format!("__ku_object_key_{target}_{field_index}");
        let value_local = format!("__ku_object_value_{target}_{field_index}");
        out.push_str(&format!(
            "  KuString {key_local} = {};\n\
               KuValue {value_local} = ku_v_null();\n",
            c_expr(key)?
        ));
        if let IrType::Array(element) = &field.ty {
            out.push_str(&format!(
                "  if (!ku_try_v_typed_array_{}({}, &{value_local})) {{\n\
                     ku_string_drop(&{key_local});\n\
                     ku_object_drop({target});\n\
                     {target} = NULL;\n\
                     ku_object_hard_fail_oom();\n\
                   }}\n",
                c_type_suffix(element)?,
                c_value_expr(field)?
            ));
        } else {
            out.push_str(&format!(
                "  {value_local} = {};\n",
                ku_value_wrap(&field.ty, &c_value_expr(field)?)?
            ));
        }
        out.push_str(&format!(
            "  if (!ku_object_try_set_owned({target}, &{key_local}, &{value_local})) {{\n\
                 ku_string_drop(&{key_local});\n\
                 ku_value_drop(&{value_local});\n\
                 ku_object_drop({target});\n\
                 {target} = NULL;\n\
                 ku_object_hard_fail_oom();\n\
               }}\n"
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
        IrType::Float => Ok(format!("ku_v_float({expr})")),
        IrType::Bool => Ok(format!("ku_v_bool({expr})")),
        IrType::Null => Ok("ku_v_null()".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok(format!("ku_v_object({expr})")),
        IrType::Named(name) if name == "__ku_value" => Ok(expr.to_string()),
        IrType::Array(element) => Ok(format!(
            "ku_v_typed_array_{}({expr})",
            c_type_suffix(element)?
        )),
        // Stage 6e-4: a function value boxed into a dynamic object is a
        // KU_FUNCTION KuValue that owns the moved closure's env reference. The
        // per-signature wrapper evaluates `expr` once (it is usually a move).
        IrType::Closure {
            params,
            param_modes,
            ret,
        } => Ok(format!(
            "ku_v_closure_{}({expr})",
            closure_signature_suffix(params, param_modes, ret)?
        )),
        _ => Err(unsupported(format!(
            "native dynamic object cannot hold a value of type {ty}"
        ))),
    }
}

fn ir_type_is_ku_value(ty: &IrType) -> bool {
    matches!(ty, IrType::Named(name) if name == "__ku_value")
}

/// Build a non-owning KuValue view for equality. Unlike `ku_value_wrap`, this
/// never consumes, clones, retains, or drops `expr`; `ku_value_equal` only reads
/// the temporary wrapper. Arrays use a dedicated typed-array comparator because
/// their homogeneous ABI is not layout-compatible with `KuValueArray`.
fn ku_value_borrow_wrap(ty: &IrType, expr: &str) -> KuResult<Option<String>> {
    let wrapped = match ty {
        IrType::Str => format!("ku_v_str({expr})"),
        IrType::Int => format!("ku_v_int({expr})"),
        IrType::Float => format!("ku_v_float({expr})"),
        IrType::Bool => format!("ku_v_bool({expr})"),
        IrType::Null => "ku_v_null()".to_string(),
        IrType::Named(name) if name == "__ku_object" => format!("ku_v_object({expr})"),
        IrType::Named(name) if name == "__ku_value" => expr.to_string(),
        IrType::Closure {
            params,
            param_modes,
            ret,
        } => format!(
            "ku_v_closure_{}({expr})",
            closure_signature_suffix(params, param_modes, ret)?
        ),
        IrType::Array(_) => return Ok(None),
        _ => return Ok(None),
    };
    Ok(Some(wrapped))
}

/// Emit a `ku_v_closure_{suffix}` per closure signature that boxes a closure
/// struct into a KU_FUNCTION `KuValue` (single-evaluation of its argument). Only
/// emitted when the program uses dynamic objects, since it depends on `KuValue`.
fn emit_closure_value_wrappers(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    if !program_uses_object(program) {
        return Ok(());
    }
    let mut types = Vec::new();
    collect_closure_types_program(program, &mut types);
    let mut emitted = false;
    for ty in &types {
        let IrType::Closure {
            params,
            param_modes,
            ret,
        } = ty
        else {
            continue;
        };
        let suffix = closure_signature_suffix(params, param_modes, ret)?;
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

/// Native fs has no distinctive handle type, so its runtime cannot be gated by
/// a type scan. Record the exact intrinsics present anywhere in the lowered IR.
/// Keeping the six flags separate lets the C emitter include only the wrappers
/// and Result/array dependencies that the artifact actually calls.
#[derive(Debug, Default, Clone, Copy)]
struct FsUsage {
    read: bool,
    try_read: bool,
    write: bool,
    try_write: bool,
    exists: bool,
    read_dir: bool,
}

impl FsUsage {
    fn any(self) -> bool {
        self.read || self.try_read || self.write || self.try_write || self.exists || self.read_dir
    }

    fn record(&mut self, name: &str) {
        match name {
            "fs.read" => self.read = true,
            "fs.try_read" => self.try_read = true,
            "fs.write" => self.write = true,
            "fs.try_write" => self.try_write = true,
            "fs.exists" => self.exists = true,
            "fs.read_dir" => self.read_dir = true,
            _ => {}
        }
    }
}

fn collect_fs_usage_expr(expr: &IrExpr, usage: &mut FsUsage) {
    match &expr.kind {
        IrExprKind::Call { callee, args, kind } => {
            if let IrCallKind::Intrinsic(name) = kind {
                usage.record(name);
            }
            collect_fs_usage_expr(callee, usage);
            for arg in args {
                collect_fs_usage_expr(arg, usage);
            }
        }
        IrExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_fs_usage_expr(value, usage);
            }
        }
        IrExprKind::Unary { expr, .. }
        | IrExprKind::Borrow(expr)
        | IrExprKind::TryUnwrap(expr)
        | IrExprKind::CellLoad(expr) => collect_fs_usage_expr(expr, usage),
        IrExprKind::Binary { left, right, .. }
        | IrExprKind::Index {
            target: left,
            index: right,
        } => {
            collect_fs_usage_expr(left, usage);
            collect_fs_usage_expr(right, usage);
        }
        IrExprKind::Field { target, .. } => collect_fs_usage_expr(target, usage),
        IrExprKind::Array(values) => {
            for value in values {
                collect_fs_usage_expr(value, usage);
            }
        }
        IrExprKind::Literal(_)
        | IrExprKind::BorrowedTemp(_)
        | IrExprKind::BorrowedParam(_)
        | IrExprKind::Local(_)
        | IrExprKind::Temp(_)
        | IrExprKind::MakeClosure { .. }
        | IrExprKind::CapturedCell(_) => {}
    }
}

fn collect_fs_usage_lvalue(target: &IrLValue, usage: &mut FsUsage) {
    match target {
        IrLValue::Local(_) => {}
        IrLValue::Index { target, index } => {
            collect_fs_usage_expr(target, usage);
            collect_fs_usage_expr(index, usage);
        }
        IrLValue::Field { target, .. } => collect_fs_usage_expr(target, usage),
    }
}

fn program_fs_usage(program: &IrProgram) -> FsUsage {
    let mut usage = FsUsage::default();
    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { value, .. }
                    | IrInst::Let { value, .. }
                    | IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value)
                    | IrInst::BindError { result: value, .. }
                    | IrInst::CellNew { init: value, .. } => {
                        collect_fs_usage_expr(value, &mut usage)
                    }
                    IrInst::BindOk { result, .. } => collect_fs_usage_expr(result, &mut usage),
                    IrInst::Store { target, value } => {
                        collect_fs_usage_lvalue(target, &mut usage);
                        collect_fs_usage_expr(value, &mut usage);
                    }
                    IrInst::CellStore { cell, value } => {
                        collect_fs_usage_expr(cell, &mut usage);
                        collect_fs_usage_expr(value, &mut usage);
                    }
                    IrInst::BeginTry { .. }
                    | IrInst::EndTry
                    | IrInst::DefineClosure { .. }
                    | IrInst::CellRelease(_)
                    | IrInst::Unsupported { .. } => {}
                }
            }
            match &block.terminator {
                IrTerminator::Branch { condition, .. } => {
                    collect_fs_usage_expr(condition, &mut usage)
                }
                IrTerminator::ForEach { iterable, .. } => {
                    collect_fs_usage_expr(iterable, &mut usage)
                }
                IrTerminator::ResultBranch { result, .. }
                | IrTerminator::JumpErr { result, .. }
                | IrTerminator::PropagateErr(result) => collect_fs_usage_expr(result, &mut usage),
                IrTerminator::Return(Some(value)) => collect_fs_usage_expr(value, &mut usage),
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Return(None)
                | IrTerminator::Unreachable => {}
                // Safepoint polling itself performs no filesystem operation.
                IrTerminator::Safepoint { .. } => {}
            }
        }
    }
    usage
}

fn emit_fs_headers(out: &mut COutput, usage: FsUsage) {
    if out.failed() {
        return;
    }
    if !usage.any() {
        return;
    }
    // This block is deliberately emitted after the optional winsock2 block:
    // including windows.h first would make a combined HTTP+fs artifact fail
    // with winsock declaration conflicts.
    out.push_str(
        "#include <limits.h>\n\
         #if defined(_WIN32)\n\
         #ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n\
         #ifndef NOMINMAX\n#define NOMINMAX\n#endif\n\
         #include <windows.h>\n#include <wchar.h>\n\
         #else\n\
         #include <dirent.h>\n#include <errno.h>\n#include <sys/stat.h>\n#include <unistd.h>\n\
         #if defined(__APPLE__)\n#include <mach-o/dyld.h>\n#endif\n\
         #endif\n\n",
    );
}

fn emit_fs_runtime(out: &mut COutput, usage: FsUsage, fs_base: &NativeFsBase) {
    if out.failed() {
        return;
    }
    if !usage.any() {
        return;
    }
    out.push_str(
        r#"#define KU_FS_MAX_PATH_BYTES ((size_t)32768)
#define KU_FS_MAX_IO_BYTES ((size_t)1000000)
#define KU_FS_MAX_DIRECTORY_ENTRIES ((size_t)10000)
#define KU_FS_MAX_DIRECTORY_OUTPUT_BYTES ((size_t)1000000)

static bool ku_fs_utf8_valid(const uint8_t* data, size_t len) {
  if (len != 0 && !data) return false;
  size_t i = 0;
  while (i < len) {
    uint8_t c = data[i];
    if (c <= 0x7f) { i++; continue; }
    if (c >= 0xc2 && c <= 0xdf) {
      if (i + 1 >= len || (data[i + 1] & 0xc0) != 0x80) return false;
      i += 2; continue;
    }
    if (c == 0xe0) {
      if (i + 2 >= len || data[i + 1] < 0xa0 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80) return false;
      i += 3; continue;
    }
    if ((c >= 0xe1 && c <= 0xec) || (c >= 0xee && c <= 0xef)) {
      if (i + 2 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80) return false;
      i += 3; continue;
    }
    if (c == 0xed) {
      if (i + 2 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x9f || (data[i + 2] & 0xc0) != 0x80) return false;
      i += 3; continue;
    }
    if (c == 0xf0) {
      if (i + 3 >= len || data[i + 1] < 0x90 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return false;
      i += 4; continue;
    }
    if (c >= 0xf1 && c <= 0xf3) {
      if (i + 3 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return false;
      i += 4; continue;
    }
    if (c == 0xf4) {
      if (i + 3 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x8f || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return false;
      i += 4; continue;
    }
    return false;
  }
  return true;
}

static bool ku_fs_path_valid(KuString path) {
  if (path.len > KU_FS_MAX_PATH_BYTES) return false;
  if (!ku_fs_utf8_valid(path.ptr, path.len)) return false;
  return path.len == 0 || memchr(path.ptr, 0, path.len) == NULL;
}

static void ku_fs_set_error(KuError* error, const char* code, const char* message) {
  if (!error) return;
  *error = ku_error_make(
      ku_string_static((const uint8_t*)"fs", 2),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}

"#,
    );

    match fs_base {
        NativeFsBase::CurrentWorkingDirectory => out.push_str(
            r#"#if defined(_WIN32)
typedef wchar_t KuFsNativeChar;
static KuFsNativeChar* ku_fs_native_path(KuString path, KuError* error, const char* code, const char* message) {
  if (!ku_fs_path_valid(path)) { ku_fs_set_error(error, code, message); return NULL; }
  if (path.len == 0) {
    wchar_t* empty = (wchar_t*)malloc(2 * sizeof(wchar_t));
    if (!empty) { ku_fs_set_error(error, code, message); return NULL; }
    empty[0] = L'.';
    empty[1] = L'\0';
    return empty;
  }
  int count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char*)path.ptr, (int)path.len, NULL, 0);
  if (count <= 0 || (size_t)count > SIZE_MAX / sizeof(wchar_t) - 1) {
    ku_fs_set_error(error, code, message); return NULL;
  }
  wchar_t* native = (wchar_t*)malloc(((size_t)count + 1) * sizeof(wchar_t));
  if (!native) { ku_fs_set_error(error, code, message); return NULL; }
  if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char*)path.ptr, (int)path.len, native, count) != count) {
    free(native); ku_fs_set_error(error, code, message); return NULL;
  }
  native[count] = L'\0';
  return native;
}
#else
typedef char KuFsNativeChar;
static KuFsNativeChar* ku_fs_native_path(KuString path, KuError* error, const char* code, const char* message) {
  if (!ku_fs_path_valid(path)) { ku_fs_set_error(error, code, message); return NULL; }
  size_t capacity = path.len == 0 ? 2 : path.len + 1;
  char* native = (char*)malloc(capacity);
  if (!native) { ku_fs_set_error(error, code, message); return NULL; }
  if (path.len) {
    memcpy(native, path.ptr, path.len);
    native[path.len] = '\0';
  } else {
    native[0] = '.';
    native[1] = '\0';
  }
  return native;
}
#endif

"#,
        ),
        NativeFsBase::ExecutableRelative(locator) => {
            out.push_str(&format!(
                "static KuString ku_fs_base_locator(void) {{ return {}; }}\n\n",
                c_static_utf8_string(locator)
            ));
            out.push_str(
                r#"#if defined(_WIN32)
typedef wchar_t KuFsNativeChar;
static wchar_t ku_fs_base_path[32769];
static size_t ku_fs_base_len = 0;
static bool ku_fs_base_ready = false;
static bool ku_fs_base_attempted = false;

static wchar_t* ku_fs_utf8_to_wide(KuString path) {
  if (path.len == 0) {
    wchar_t* empty = (wchar_t*)malloc(sizeof(wchar_t));
    if (empty) empty[0] = L'\0';
    return empty;
  }
  if (path.len > (size_t)INT_MAX) return NULL;
  int count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char*)path.ptr, (int)path.len, NULL, 0);
  if (count <= 0 || (size_t)count > KU_FS_MAX_PATH_BYTES || (size_t)count > SIZE_MAX / sizeof(wchar_t) - 1) return NULL;
  wchar_t* native = (wchar_t*)malloc(((size_t)count + 1) * sizeof(wchar_t));
  if (!native) return NULL;
  if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char*)path.ptr, (int)path.len, native, count) != count) {
    free(native); return NULL;
  }
  native[count] = L'\0';
  return native;
}

static bool ku_fs_wide_separator(wchar_t value) {
  return value == L'\\' || value == L'/';
}

static bool ku_fs_wide_drive_prefixed(const wchar_t* path, size_t len) {
  return len >= 2 && ((path[0] >= L'A' && path[0] <= L'Z') ||
      (path[0] >= L'a' && path[0] <= L'z')) && path[1] == L':';
}

static bool ku_fs_wide_double_root(const wchar_t* path, size_t len) {
  return len >= 2 && ku_fs_wide_separator(path[0]) && ku_fs_wide_separator(path[1]);
}

static bool ku_fs_wide_verbatim_or_device(const wchar_t* path, size_t len) {
  return len >= 4 && ku_fs_wide_double_root(path, len) &&
      (path[2] == L'?' || path[2] == L'.') && ku_fs_wide_separator(path[3]);
}

/* Return the end of two non-empty components separated by a slash. For a
   normal UNC path component_start=2 (server + share); for \\?\UNC it is 8. */
static size_t ku_fs_wide_two_component_prefix_len(
    const wchar_t* path, size_t len, size_t component_start) {
  size_t first_end = component_start;
  while (first_end < len && !ku_fs_wide_separator(path[first_end])) first_end++;
  if (first_end == component_start || first_end >= len) return 0;
  size_t second_start = first_end + 1;
  size_t second_end = second_start;
  while (second_end < len && !ku_fs_wide_separator(path[second_end])) second_end++;
  return second_end == second_start ? 0 : second_end;
}

static size_t ku_fs_wide_unc_prefix_len(const wchar_t* path, size_t len) {
  if (!ku_fs_wide_double_root(path, len) || ku_fs_wide_verbatim_or_device(path, len)) return 0;
  return ku_fs_wide_two_component_prefix_len(path, len, 2);
}

static bool ku_fs_wide_unc(const wchar_t* path, size_t len) {
  return ku_fs_wide_unc_prefix_len(path, len) != 0;
}

static bool ku_fs_wide_fully_qualified(const wchar_t* path, size_t len) {
  return ku_fs_wide_verbatim_or_device(path, len) || ku_fs_wide_unc(path, len) ||
      (ku_fs_wide_drive_prefixed(path, len) && len >= 3 && ku_fs_wide_separator(path[2]));
}

static bool ku_fs_wide_drive_relative(const wchar_t* path, size_t len) {
  return ku_fs_wide_drive_prefixed(path, len) &&
      (len < 3 || !ku_fs_wide_separator(path[2]));
}

static bool ku_fs_wide_root_relative(const wchar_t* path, size_t len) {
  return len != 0 && ku_fs_wide_separator(path[0]) &&
      !ku_fs_wide_fully_qualified(path, len);
}

/* A rooted-but-unprefixed Windows path (`\foo` or `/foo`) keeps the base
   path's drive/UNC prefix under Rust PathBuf::join, but replaces its directory
   components. Return the prefix length that must be preserved. */
static size_t ku_fs_wide_base_prefix_len(const wchar_t* path, size_t len) {
  if (ku_fs_wide_drive_prefixed(path, len)) return 2;
  if (!ku_fs_wide_double_root(path, len)) return 0;

  /* Verbatim UNC: \\?\UNC\server\share\... keeps the whole
     \\?\UNC\server\share prefix, not merely \\?\UNC. */
  if (len >= 8 && path[2] == L'?' && ku_fs_wide_separator(path[3]) &&
      (path[4] == L'U' || path[4] == L'u') &&
      (path[5] == L'N' || path[5] == L'n') &&
      (path[6] == L'C' || path[6] == L'c') && ku_fs_wide_separator(path[7])) {
    return ku_fs_wide_two_component_prefix_len(path, len, 8);
  }
  return ku_fs_wide_two_component_prefix_len(path, len, 2);
}

static bool ku_fs_init_base(void) {
  if (ku_fs_base_ready) return true;
  if (ku_fs_base_attempted) return false;
  ku_fs_base_attempted = true;

  wchar_t executable[32769];
  DWORD executable_len = GetModuleFileNameW(NULL, executable, (DWORD)32769);
  if (executable_len == 0 || executable_len > (DWORD)KU_FS_MAX_PATH_BYTES) return false;
  executable[executable_len] = L'\0';
  size_t directory_len = (size_t)executable_len;
  while (directory_len != 0 && executable[directory_len - 1] != L'\\' && executable[directory_len - 1] != L'/') directory_len--;
  if (directory_len == 0) return false;

  KuString locator = ku_fs_base_locator();
  if (!ku_fs_path_valid(locator)) return false;
  wchar_t* native_locator = ku_fs_utf8_to_wide(locator);
  if (!native_locator) return false;
  size_t locator_len = wcslen(native_locator);
  if (ku_fs_wide_fully_qualified(native_locator, locator_len) ||
      ku_fs_wide_drive_relative(native_locator, locator_len) ||
      ku_fs_wide_root_relative(native_locator, locator_len) ||
      locator_len > KU_FS_MAX_PATH_BYTES - directory_len) {
    free(native_locator); return false;
  }
  size_t joined_len = directory_len + locator_len;
  wchar_t* joined = (wchar_t*)malloc((joined_len + 1) * sizeof(wchar_t));
  if (!joined) { free(native_locator); return false; }
  memcpy(joined, executable, directory_len * sizeof(wchar_t));
  if (locator_len) memcpy(joined + directory_len, native_locator, locator_len * sizeof(wchar_t));
  joined[joined_len] = L'\0';
  free(native_locator);

  DWORD base_len = GetFullPathNameW(joined, (DWORD)32769, ku_fs_base_path, NULL);
  free(joined);
  if (base_len == 0 || base_len > (DWORD)KU_FS_MAX_PATH_BYTES) return false;
  ku_fs_base_len = (size_t)base_len;
  ku_fs_base_ready = true;
  return true;
}

static KuFsNativeChar* ku_fs_native_path(KuString path, KuError* error, const char* code, const char* message) {
  if (!ku_fs_path_valid(path)) { ku_fs_set_error(error, code, message); return NULL; }
  wchar_t* native = ku_fs_utf8_to_wide(path);
  if (!native) { ku_fs_set_error(error, code, message); return NULL; }
  size_t native_len = wcslen(native);
  /* `C:\\foo` and UNC are fully qualified. `C:foo` is drive-relative and
     PathBuf::join replaces the base prefix with it, so it likewise stays
     unchanged for the CRT to resolve against that drive's current directory. */
  if (ku_fs_wide_fully_qualified(native, native_len) ||
      ku_fs_wide_drive_relative(native, native_len)) return native;
  if (!ku_fs_base_ready && !ku_fs_init_base()) {
    free(native); ku_fs_set_error(error, code, message); return NULL;
  }
  if (ku_fs_wide_root_relative(native, native_len)) {
    size_t prefix_len = ku_fs_wide_base_prefix_len(ku_fs_base_path, ku_fs_base_len);
    if (prefix_len == 0 || native_len > KU_FS_MAX_PATH_BYTES - prefix_len) {
      free(native); ku_fs_set_error(error, code, message); return NULL;
    }
    size_t joined_len = prefix_len + native_len;
    wchar_t* joined = (wchar_t*)malloc((joined_len + 1) * sizeof(wchar_t));
    if (!joined) { free(native); ku_fs_set_error(error, code, message); return NULL; }
    memcpy(joined, ku_fs_base_path, prefix_len * sizeof(wchar_t));
    memcpy(joined + prefix_len, native, native_len * sizeof(wchar_t));
    joined[joined_len] = L'\0';
    free(native);
    return joined;
  }
  bool needs_separator = ku_fs_base_len != 0 && ku_fs_base_path[ku_fs_base_len - 1] != L'\\' && ku_fs_base_path[ku_fs_base_len - 1] != L'/';
  size_t separator_len = needs_separator ? 1 : 0;
  if (native_len > KU_FS_MAX_PATH_BYTES - ku_fs_base_len || separator_len > KU_FS_MAX_PATH_BYTES - ku_fs_base_len - native_len) {
    free(native); ku_fs_set_error(error, code, message); return NULL;
  }
  size_t joined_len = ku_fs_base_len + separator_len + native_len;
  wchar_t* joined = (wchar_t*)malloc((joined_len + 1) * sizeof(wchar_t));
  if (!joined) { free(native); ku_fs_set_error(error, code, message); return NULL; }
  memcpy(joined, ku_fs_base_path, ku_fs_base_len * sizeof(wchar_t));
  if (needs_separator) joined[ku_fs_base_len] = L'\\';
  if (native_len) memcpy(joined + ku_fs_base_len + separator_len, native, native_len * sizeof(wchar_t));
  joined[joined_len] = L'\0';
  free(native);
  return joined;
}
#else
typedef char KuFsNativeChar;
static char ku_fs_base_path[32769];
static size_t ku_fs_base_len = 0;
static bool ku_fs_base_ready = false;
static bool ku_fs_base_attempted = false;

static char* ku_fs_executable_path(void) {
#if defined(__APPLE__)
  size_t capacity = 256;
  while (capacity <= KU_FS_MAX_PATH_BYTES + 1) {
    char* buffer = (char*)malloc(capacity);
    if (!buffer) return NULL;
    uint32_t native_capacity = (uint32_t)capacity;
    if (_NSGetExecutablePath(buffer, &native_capacity) == 0) {
      size_t len = strlen(buffer);
      if (len <= KU_FS_MAX_PATH_BYTES) return buffer;
      free(buffer); return NULL;
    }
    free(buffer);
    size_t next = (size_t)native_capacity;
    if (next <= capacity) next = capacity <= (KU_FS_MAX_PATH_BYTES + 1) / 2 ? capacity * 2 : KU_FS_MAX_PATH_BYTES + 1;
    if (next > KU_FS_MAX_PATH_BYTES + 1) next = KU_FS_MAX_PATH_BYTES + 1;
    if (next <= capacity) return NULL;
    capacity = next;
  }
  return NULL;
#elif defined(__linux__)
  size_t capacity = 256;
  while (capacity <= KU_FS_MAX_PATH_BYTES + 1) {
    char* buffer = (char*)malloc(capacity + 1);
    if (!buffer) return NULL;
    ssize_t len = readlink("/proc/self/exe", buffer, capacity);
    if (len >= 0 && (size_t)len < capacity) {
      if ((size_t)len > KU_FS_MAX_PATH_BYTES) { free(buffer); return NULL; }
      buffer[len] = '\0';
      return buffer;
    }
    free(buffer);
    if (len < 0) return NULL;
    size_t next = capacity <= (KU_FS_MAX_PATH_BYTES + 1) / 2 ? capacity * 2 : KU_FS_MAX_PATH_BYTES + 1;
    if (next > KU_FS_MAX_PATH_BYTES + 1) next = KU_FS_MAX_PATH_BYTES + 1;
    if (next <= capacity) return NULL;
    capacity = next;
  }
  return NULL;
#else
  return NULL;
#endif
}

static bool ku_fs_init_base(void) {
  if (ku_fs_base_ready) return true;
  if (ku_fs_base_attempted) return false;
  ku_fs_base_attempted = true;

  char* executable = ku_fs_executable_path();
  if (!executable) return false;
  char* resolved_executable = realpath(executable, NULL);
  free(executable);
  if (!resolved_executable) return false;
  size_t executable_len = strlen(resolved_executable);
  if (executable_len > KU_FS_MAX_PATH_BYTES) { free(resolved_executable); return false; }
  char* separator = strrchr(resolved_executable, '/');
  if (!separator) { free(resolved_executable); return false; }
  size_t directory_len = separator == resolved_executable ? 1 : (size_t)(separator - resolved_executable);

  KuString locator = ku_fs_base_locator();
  if (!ku_fs_path_valid(locator) || (locator.len != 0 && locator.ptr[0] == '/')) {
    free(resolved_executable); return false;
  }
  bool needs_separator = directory_len != 0 && resolved_executable[directory_len - 1] != '/';
  size_t separator_len = needs_separator ? 1 : 0;
  if (locator.len > KU_FS_MAX_PATH_BYTES - directory_len || separator_len > KU_FS_MAX_PATH_BYTES - directory_len - locator.len) {
    free(resolved_executable); return false;
  }
  size_t joined_len = directory_len + separator_len + locator.len;
  char* joined = (char*)malloc(joined_len + 1);
  if (!joined) { free(resolved_executable); return false; }
  memcpy(joined, resolved_executable, directory_len);
  free(resolved_executable);
  if (needs_separator) joined[directory_len] = '/';
  if (locator.len) memcpy(joined + directory_len + separator_len, locator.ptr, locator.len);
  joined[joined_len] = '\0';

  char* resolved_base = realpath(joined, NULL);
  free(joined);
  if (!resolved_base) return false;
  size_t base_len = strlen(resolved_base);
  if (base_len > KU_FS_MAX_PATH_BYTES) { free(resolved_base); return false; }
  memcpy(ku_fs_base_path, resolved_base, base_len + 1);
  free(resolved_base);
  ku_fs_base_len = base_len;
  ku_fs_base_ready = true;
  return true;
}

static KuFsNativeChar* ku_fs_native_path(KuString path, KuError* error, const char* code, const char* message) {
  if (!ku_fs_path_valid(path)) { ku_fs_set_error(error, code, message); return NULL; }
  if (path.len != 0 && path.ptr[0] == '/') {
    char* absolute = (char*)malloc(path.len + 1);
    if (!absolute) { ku_fs_set_error(error, code, message); return NULL; }
    memcpy(absolute, path.ptr, path.len);
    absolute[path.len] = '\0';
    return absolute;
  }
  if (!ku_fs_base_ready && !ku_fs_init_base()) {
    ku_fs_set_error(error, code, message); return NULL;
  }
  bool needs_separator = ku_fs_base_len != 0 && ku_fs_base_path[ku_fs_base_len - 1] != '/';
  size_t separator_len = needs_separator ? 1 : 0;
  if (path.len > KU_FS_MAX_PATH_BYTES - ku_fs_base_len || separator_len > KU_FS_MAX_PATH_BYTES - ku_fs_base_len - path.len) {
    ku_fs_set_error(error, code, message); return NULL;
  }
  size_t joined_len = ku_fs_base_len + separator_len + path.len;
  char* joined = (char*)malloc(joined_len + 1);
  if (!joined) { ku_fs_set_error(error, code, message); return NULL; }
  memcpy(joined, ku_fs_base_path, ku_fs_base_len);
  if (needs_separator) joined[ku_fs_base_len] = '/';
  if (path.len) memcpy(joined + ku_fs_base_len + separator_len, path.ptr, path.len);
  joined[joined_len] = '\0';
  return joined;
}
#endif

"#,
            );
        }
        NativeFsBase::Unavailable(_) => unreachable!("fs usage was validated before emission"),
    }

    if usage.read || usage.try_read {
        out.push_str(
            r#"static bool ku_fs_read_impl(KuString path, KuString* value, KuError* error) {
  if (value) *value = (KuString){0};
  if (error) *error = (KuError){0};
  KuFsNativeChar* native = ku_fs_native_path(path, error, "read_failed", "failed to read: invalid path");
  if (!native) return false;
#if defined(_WIN32)
  FILE* file = _wfopen(native, L"rb");
#else
  FILE* file = fopen(native, "rb");
#endif
  free(native);
  if (!file) { ku_fs_set_error(error, "read_failed", "failed to read file"); return false; }

  size_t capacity = 4096;
  uint8_t* data = (uint8_t*)malloc(capacity);
  if (!data) {
    (void)fclose(file);
    ku_fs_set_error(error, "read_failed", "failed to read: out of memory");
    return false;
  }
  size_t len = 0;
  bool failed = false;
  while (!failed) {
    if (len == KU_FS_MAX_IO_BYTES) {
      uint8_t extra = 0;
      size_t got = fread(&extra, 1, 1, file);
      if (got == 1) {
        ku_fs_set_error(error, "file_too_large", "failed to read: file is too large");
        failed = true;
      } else if (ferror(file)) {
        ku_fs_set_error(error, "read_failed", "failed to read file");
        failed = true;
      }
      break;
    }
    if (len == capacity) {
      size_t next = capacity <= KU_FS_MAX_IO_BYTES / 2 ? capacity * 2 : KU_FS_MAX_IO_BYTES;
      if (next <= capacity) {
        ku_fs_set_error(error, "read_failed", "failed to read: buffer limit reached");
        failed = true;
        break;
      }
      uint8_t* grown = (uint8_t*)realloc(data, next);
      if (!grown) {
        ku_fs_set_error(error, "read_failed", "failed to read: out of memory");
        failed = true;
        break;
      }
      data = grown;
      capacity = next;
    }
    size_t got = fread(data + len, 1, capacity - len, file);
    if (got != 0) {
      len += got;
      continue;
    }
    if (ferror(file)) {
      ku_fs_set_error(error, "read_failed", "failed to read file");
      failed = true;
    }
    break;
  }
  if (fclose(file) != 0 && !failed) {
    ku_fs_set_error(error, "read_failed", "failed to close file after reading");
    failed = true;
  }
  if (!failed && !ku_fs_utf8_valid(data, len)) {
    ku_fs_set_error(error, "read_failed", "failed to read: file is not valid UTF-8");
    failed = true;
  }
  if (failed) {
    free(data);
    return false;
  }
  if (value) *value = (KuString){ data, len, capacity, KU_STRING_OWNED };
  else free(data);
  return true;
}

"#,
        );
        if usage.read {
            out.push_str(
                r#"static KuResult_str ku_fs_read(KuString path) {
  KuString value = (KuString){0};
  KuError error = (KuError){0};
  if (ku_fs_read_impl(path, &value, &error)) return (KuResult_str){ true, value, (KuError){0} };
  return (KuResult_str){ false, (KuString){0}, error };
}

"#,
            );
        }
        if usage.try_read {
            out.push_str(
                r#"static KuResult_str ku_fs_try_read(KuString path) {
  KuString value = (KuString){0};
  KuError error = (KuError){0};
  if (ku_fs_read_impl(path, &value, &error)) return (KuResult_str){ true, value, (KuError){0} };
  return (KuResult_str){ false, (KuString){0}, error };
}

"#,
            );
        }
    }

    if usage.write || usage.try_write {
        out.push_str(
            r#"static bool ku_fs_write_impl(KuString path, KuString content, KuError* error) {
  if (error) *error = (KuError){0};
  if (content.len > KU_FS_MAX_IO_BYTES) {
    ku_fs_set_error(error, "content_too_large", "failed to write: content is too large");
    return false;
  }
  if (!ku_fs_utf8_valid(content.ptr, content.len)) {
    ku_fs_set_error(error, "write_failed", "failed to write: content is not valid UTF-8");
    return false;
  }
  KuFsNativeChar* native = ku_fs_native_path(path, error, "write_failed", "failed to write: invalid path");
  if (!native) return false;
#if defined(_WIN32)
  FILE* file = _wfopen(native, L"wb");
#else
  FILE* file = fopen(native, "wb");
#endif
  free(native);
  if (!file) { ku_fs_set_error(error, "write_failed", "failed to open file for writing"); return false; }

  bool failed = false;
  size_t offset = 0;
  while (offset < content.len) {
    size_t wrote = fwrite(content.ptr + offset, 1, content.len - offset, file);
    if (wrote == 0) {
      ku_fs_set_error(error, "write_failed", "failed to write file");
      failed = true;
      break;
    }
    offset += wrote;
  }
  if (fclose(file) != 0 && !failed) {
    ku_fs_set_error(error, "write_failed", "failed to close file after writing");
    failed = true;
  }
  return !failed;
}

"#,
        );
        if usage.write {
            out.push_str(
                r#"static KuResult_null ku_fs_write(KuString path, KuString content) {
  KuError error = (KuError){0};
  if (ku_fs_write_impl(path, content, &error)) return (KuResult_null){ true, 0, (KuError){0} };
  return (KuResult_null){ false, 0, error };
}

"#,
            );
        }
        if usage.try_write {
            out.push_str(
                r#"static KuResult_null ku_fs_try_write(KuString path, KuString content) {
  KuError error = (KuError){0};
  if (ku_fs_write_impl(path, content, &error)) return (KuResult_null){ true, 0, (KuError){0} };
  return (KuResult_null){ false, 0, error };
}

"#,
            );
        }
    }

    if usage.exists {
        out.push_str(
            r#"static bool ku_fs_exists(KuString path) {
  KuFsNativeChar* native = ku_fs_native_path(path, NULL, "", "");
  if (!native) return false;
#if defined(_WIN32)
  HANDLE handle = CreateFileW(
      native,
      0,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
      NULL,
      OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS,
      NULL);
  bool exists = handle != INVALID_HANDLE_VALUE;
  if (exists) CloseHandle(handle);
#else
  struct stat metadata;
  bool exists = stat(native, &metadata) == 0;
#endif
  free(native);
  return exists;
}

"#,
        );
    }

    if usage.read_dir {
        out.push_str(
            r#"typedef struct KuFsDirBuilder {
  KuArray_str values;
  size_t capacity;
  size_t output_bytes;
} KuFsDirBuilder;

static void ku_fs_dir_builder_drop(KuFsDirBuilder* builder) {
  if (!builder) return;
  for (size_t i = 0; i < builder->values.len; i++) ku_string_drop(&builder->values.data[i]);
  free(builder->values.data);
  *builder = (KuFsDirBuilder){0};
}

static bool ku_fs_dir_builder_push(KuFsDirBuilder* builder, KuString value, KuError* error) {
  if (builder->values.len >= KU_FS_MAX_DIRECTORY_ENTRIES) {
    ku_string_drop(&value);
    ku_fs_set_error(error, "read_dir_failed", "failed to read directory: directory has too many entries");
    return false;
  }
  if (value.len > KU_FS_MAX_DIRECTORY_OUTPUT_BYTES - builder->output_bytes) {
    ku_string_drop(&value);
    ku_fs_set_error(error, "read_dir_failed", "failed to read directory: directory listing is too large");
    return false;
  }
  if (builder->values.len == builder->capacity) {
    size_t next = builder->capacity == 0 ? 16 : builder->capacity * 2;
    if (next > KU_FS_MAX_DIRECTORY_ENTRIES) next = KU_FS_MAX_DIRECTORY_ENTRIES;
    KuString* grown = (KuString*)realloc(builder->values.data, next * sizeof(KuString));
    if (!grown) {
      ku_string_drop(&value);
      ku_fs_set_error(error, "read_dir_failed", "failed to read directory: out of memory");
      return false;
    }
    builder->values.data = grown;
    builder->capacity = next;
  }
  builder->values.data[builder->values.len++] = value;
  builder->output_bytes += value.len;
  return true;
}

static int ku_fs_path_compare(const void* left_ptr, const void* right_ptr) {
  const KuString* left = (const KuString*)left_ptr;
  const KuString* right = (const KuString*)right_ptr;
  size_t common = left->len < right->len ? left->len : right->len;
  int order = common == 0 ? 0 : memcmp(left->ptr, right->ptr, common);
  if (order != 0) return order;
  if (left->len < right->len) return -1;
  if (left->len > right->len) return 1;
  return 0;
}

static KuResult_array_str ku_fs_read_dir_error(KuFsDirBuilder* builder, KuError error) {
  ku_fs_dir_builder_drop(builder);
  return (KuResult_array_str){ false, (KuArray_str){0}, error };
}

#if defined(_WIN32)
static bool ku_fs_wide_to_string(const wchar_t* wide, size_t len, KuString* value, KuError* error) {
  if (len > KU_FS_MAX_PATH_BYTES) {
    ku_fs_set_error(error, "read_dir_failed", "failed to read directory: directory entry path is too long");
    return false;
  }
  if (len == 0) {
    uint8_t* data = (uint8_t*)malloc(1);
    if (!data) {
      ku_fs_set_error(error, "read_dir_failed", "failed to read directory: out of memory");
      return false;
    }
    *value = (KuString){ data, 0, 1, KU_STRING_OWNED };
    return true;
  }
  int bytes = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, wide, (int)len, NULL, 0, NULL, NULL);
  if (bytes <= 0) {
    ku_fs_set_error(error, "read_dir_failed", "failed to read directory: entry path is not valid UTF-8");
    return false;
  }
  if ((size_t)bytes > KU_FS_MAX_PATH_BYTES) {
    ku_fs_set_error(error, "read_dir_failed", "failed to read directory: directory entry path is too long");
    return false;
  }
  uint8_t* data = (uint8_t*)malloc((size_t)bytes);
  if (!data) {
    ku_fs_set_error(error, "read_dir_failed", "failed to read directory: out of memory");
    return false;
  }
  if (WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, wide, (int)len, (char*)data, bytes, NULL, NULL) != bytes) {
    free(data);
    ku_fs_set_error(error, "read_dir_failed", "failed to read directory: entry path is not valid UTF-8");
    return false;
  }
  *value = (KuString){ data, (size_t)bytes, (size_t)bytes, KU_STRING_OWNED };
  return true;
}

static KuResult_array_str ku_fs_read_dir(KuString path) {
  KuFsDirBuilder builder = (KuFsDirBuilder){0};
  KuError error = (KuError){0};
  KuFsNativeChar* native = ku_fs_native_path(path, &error, "read_dir_failed", "failed to read directory: invalid path");
  if (!native) return ku_fs_read_dir_error(&builder, error);

  size_t base_len = wcslen(native);
  bool separator = base_len != 0 && (native[base_len - 1] == L'\\' || native[base_len - 1] == L'/');
  if (base_len > SIZE_MAX - 3) {
    free(native);
    ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: path is too long");
    return ku_fs_read_dir_error(&builder, error);
  }
  size_t pattern_len = base_len + (separator ? 0 : 1) + 1;
  wchar_t* pattern = (wchar_t*)malloc((pattern_len + 1) * sizeof(wchar_t));
  if (!pattern) {
    free(native);
    ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: out of memory");
    return ku_fs_read_dir_error(&builder, error);
  }
  if (base_len) memcpy(pattern, native, base_len * sizeof(wchar_t));
  size_t pattern_at = base_len;
  if (!separator) pattern[pattern_at++] = L'\\';
  pattern[pattern_at++] = L'*';
  pattern[pattern_at] = L'\0';

  WIN32_FIND_DATAW entry;
  HANDLE handle = FindFirstFileW(pattern, &entry);
  free(pattern);
  if (handle == INVALID_HANDLE_VALUE) {
    free(native);
    ku_fs_set_error(&error, "read_dir_failed", "failed to read directory");
    return ku_fs_read_dir_error(&builder, error);
  }

  bool failed = false;
  for (;;) {
    const wchar_t* name = entry.cFileName;
    bool dot = wcscmp(name, L".") == 0 || wcscmp(name, L"..") == 0;
    if (!dot) {
      size_t name_len = wcslen(name);
      if (base_len > SIZE_MAX - name_len - 2) {
        ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: entry path is too long");
        failed = true;
      } else {
        size_t full_len = base_len + (separator ? 0 : 1) + name_len;
        wchar_t* full = (wchar_t*)malloc((full_len + 1) * sizeof(wchar_t));
        if (!full) {
          ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: out of memory");
          failed = true;
        } else {
          if (base_len) memcpy(full, native, base_len * sizeof(wchar_t));
          size_t full_at = base_len;
          if (!separator) full[full_at++] = L'\\';
          if (name_len) memcpy(full + full_at, name, name_len * sizeof(wchar_t));
          full[full_len] = L'\0';
          KuString output = (KuString){0};
          if (!ku_fs_wide_to_string(full, full_len, &output, &error) ||
              !ku_fs_dir_builder_push(&builder, output, &error)) failed = true;
          free(full);
        }
      }
    }
    if (failed) break;
    if (!FindNextFileW(handle, &entry)) {
      DWORD finish = GetLastError();
      if (finish != ERROR_NO_MORE_FILES) {
        ku_fs_set_error(&error, "read_dir_failed", "failed while reading directory");
        failed = true;
      }
      break;
    }
  }
  if (!FindClose(handle) && !failed) {
    ku_fs_set_error(&error, "read_dir_failed", "failed to close directory");
    failed = true;
  }
  free(native);
  if (failed) return ku_fs_read_dir_error(&builder, error);
  if (builder.values.len > 1) qsort(builder.values.data, builder.values.len, sizeof(KuString), ku_fs_path_compare);
  return (KuResult_array_str){ true, builder.values, (KuError){0} };
}
#else
static KuResult_array_str ku_fs_read_dir(KuString path) {
  KuFsDirBuilder builder = (KuFsDirBuilder){0};
  KuError error = (KuError){0};
  KuFsNativeChar* native = ku_fs_native_path(path, &error, "read_dir_failed", "failed to read directory: invalid path");
  if (!native) return ku_fs_read_dir_error(&builder, error);
  DIR* directory = opendir(native);
  if (!directory) {
    free(native);
    ku_fs_set_error(&error, "read_dir_failed", "failed to read directory");
    return ku_fs_read_dir_error(&builder, error);
  }

  size_t base_len = strlen(native);
  bool separator = base_len != 0 && native[base_len - 1] == '/';
  bool failed = false;
  for (;;) {
    errno = 0;
    struct dirent* entry = readdir(directory);
    if (!entry) {
      if (errno != 0) {
        ku_fs_set_error(&error, "read_dir_failed", "failed while reading directory");
        failed = true;
      }
      break;
    }
    const char* name = entry->d_name;
    if ((name[0] == '.' && name[1] == '\0') ||
        (name[0] == '.' && name[1] == '.' && name[2] == '\0')) continue;
    size_t name_len = strlen(name);
    if (!ku_fs_utf8_valid((const uint8_t*)name, name_len)) {
      ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: entry path is not valid UTF-8");
      failed = true;
      break;
    }
    if (base_len > SIZE_MAX - name_len - 2) {
      ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: entry path is too long");
      failed = true;
      break;
    }
    size_t full_len = base_len + (separator ? 0 : 1) + name_len;
    if (full_len > KU_FS_MAX_PATH_BYTES) {
      ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: entry path is too long");
      failed = true;
      break;
    }
    uint8_t* full = (uint8_t*)malloc(full_len ? full_len : 1);
    if (!full) {
      ku_fs_set_error(&error, "read_dir_failed", "failed to read directory: out of memory");
      failed = true;
      break;
    }
    if (base_len) memcpy(full, native, base_len);
    size_t full_at = base_len;
    if (!separator) full[full_at++] = '/';
    if (name_len) memcpy(full + full_at, name, name_len);
    KuString output = (KuString){ full, full_len, full_len ? full_len : 1, KU_STRING_OWNED };
    if (!ku_fs_dir_builder_push(&builder, output, &error)) {
      failed = true;
      break;
    }
  }
  if (closedir(directory) != 0 && !failed) {
    ku_fs_set_error(&error, "read_dir_failed", "failed to close directory");
    failed = true;
  }
  free(native);
  if (failed) return ku_fs_read_dir_error(&builder, error);
  if (builder.values.len > 1) qsort(builder.values.data, builder.values.len, sizeof(KuString), ku_fs_path_compare);
  return (KuResult_array_str){ true, builder.values, (KuError){0} };
}
#endif

"#,
        );
    }
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
        IrType::Named(name) => name == "__ku_object" || name == "__ku_value",
        IrType::Array(inner) | IrType::Result(inner) => ir_type_uses_object(inner),
        _ => false,
    }
}

fn inst_uses_object(inst: &IrInst) -> bool {
    match inst {
        IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
            ir_type_uses_object(ty) || expr_uses_object(value)
        }
        IrInst::BindOk { ty, result, .. } => ir_type_uses_object(ty) || expr_uses_object(result),
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
                if name == "__ku_object"
                    || name == "json.stringify"
                    || name == "json.parse"
                    || name == "json.try_parse")
                || expr_uses_object(callee)
                || args.iter().any(expr_uses_object)
        }
        IrExprKind::Binary { left, right, .. } => expr_uses_object(left) || expr_uses_object(right),
        IrExprKind::Unary { expr, .. } => expr_uses_object(expr),
        IrExprKind::Index { target, index } => expr_uses_object(target) || expr_uses_object(index),
        IrExprKind::Field { target, .. } => expr_uses_object(target),
        IrExprKind::Array(values) => values.iter().any(expr_uses_object),
        IrExprKind::StructLiteral { fields, .. } => fields.iter().any(|(_, v)| expr_uses_object(v)),
        IrExprKind::TryUnwrap(inner) => expr_uses_object(inner),
        _ => false,
    }
}

/// Stage 8a: true when the program uses the native HTTP server (so the portable
/// socket runtime and response/request structs are emitted; Windows artifacts
/// additionally link `ws2_32`). The synthetic HTTP types only arise from the HTTP
/// intrinsics, so a type scan over every temp/param/return type catches every such
/// program.
fn program_uses_http(program: &IrProgram) -> bool {
    program.functions.iter().any(|function| {
        ir_type_uses_http(&function.return_type)
            || function.params.iter().any(|p| ir_type_uses_http(&p.ty))
            || function
                .blocks
                .iter()
                .any(|block| block.instructions.iter().any(inst_uses_http))
    })
}

/// True when any type in the program is a `pg` handle — the only way those arise is a
/// `pg` intrinsic, so a type scan detects every program that needs the libpq binding.
fn program_uses_pg(program: &IrProgram) -> bool {
    fn ty_uses_pg(ty: &IrType) -> bool {
        match ty {
            IrType::Named(name) => name.starts_with("__ku_pg_"),
            IrType::Array(inner) | IrType::Result(inner) | IrType::Cell(inner) => ty_uses_pg(inner),
            IrType::Closure { params, ret, .. } => params.iter().any(ty_uses_pg) || ty_uses_pg(ret),
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
            || function
                .blocks
                .iter()
                .any(|block| block.instructions.iter().any(inst_uses_pg))
    })
}

/// True when the program uses a `mysql` handle (needs the libmysqlclient binding).
fn program_uses_mysql(program: &IrProgram) -> bool {
    fn ty(t: &IrType) -> bool {
        match t {
            IrType::Named(name) => name.starts_with("__ku_mysql_"),
            IrType::Array(i) | IrType::Result(i) | IrType::Cell(i) => ty(i),
            IrType::Closure { params, ret, .. } => params.iter().any(ty) || ty(ret),
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

/// Include libmysqlclient's public ABI before Result declarations. MYSQL_BIND is
/// intentionally never redeclared by Ku: its layout differs between client
/// releases, so a hand-written shadow struct would be memory-unsafe.
fn emit_mysql_types(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_mysql(program) {
        return;
    }
    out.push_str(concat!(
        "#if defined(KU_MYSQL_SELECTED_HEADER)\n# include KU_MYSQL_SELECTED_HEADER\n",
        "#elif defined(KU_MYSQL_FAKE_CLIENT)\n# include \"mysql.h\"\n",
        "#elif defined(__has_include)\n",
        "# if __has_include(<mysql.h>)\n#  include <mysql.h>\n",
        "# elif __has_include(<mysql/mysql.h>)\n#  include <mysql/mysql.h>\n",
        "# elif __has_include(<mariadb/mysql.h>)\n#  include <mariadb/mysql.h>\n",
        "# else\n#  error \"std.mysql requires libmysqlclient development headers\"\n# endif\n",
        "#else\n# include <mysql.h>\n#endif\n#include <limits.h>\n",
        "#if defined(MARIADB_BASE_VERSION)\n",
        "# if !defined(MARIADB_PACKAGE_VERSION_ID)\n",
        "#  error \"std.mysql requires a MariaDB Connector/C package version macro\"\n",
        "# elif !defined(MARIADB_VERSION_ID)\n",
        "#  error \"std.mysql requires a MariaDB client compatibility version macro\"\n",
        "# elif MARIADB_PACKAGE_VERSION_ID < 30100 || MARIADB_PACKAGE_VERSION_ID >= 40000\n",
        "#  error \"std.mysql requires MariaDB Connector/C 3.1.x through 3.x\"\n",
        "# endif\n",
        "# define KU_MYSQL_HEADER_FAMILY_MARIADB 1\n",
        "# define KU_MYSQL_HEADER_ABI_MAJOR (MARIADB_PACKAGE_VERSION_ID / 10000UL)\n",
        "#else\n",
        "# if !defined(MYSQL_VERSION_ID)\n",
        "#  error \"std.mysql requires a supported MySQL or MariaDB client header\"\n",
        "# elif MYSQL_VERSION_ID < 50703\n",
        "#  error \"std.mysql requires mysql_reset_connection (MySQL 5.7.3 or newer)\"\n",
        "# endif\n",
        "# define KU_MYSQL_HEADER_FAMILY_MARIADB 0\n",
        "# define KU_MYSQL_HEADER_ABI_MAJOR (MYSQL_VERSION_ID / 10000UL)\n",
        "#endif\n",
        "#if defined(KU_MYSQL_SELECTED_HEADER) && !defined(KU_MYSQL_EXPECT_HEADER_FAMILY)\n",
        "# error \"selected MySQL header is missing its client-family contract\"\n",
        "#elif !defined(KU_MYSQL_SELECTED_HEADER) && defined(KU_MYSQL_EXPECT_HEADER_FAMILY)\n",
        "# error \"MySQL client-family contract is missing its selected header\"\n",
        "#elif defined(KU_MYSQL_EXPECT_HEADER_FAMILY)\n",
        "# if KU_MYSQL_EXPECT_HEADER_FAMILY != 0 && KU_MYSQL_EXPECT_HEADER_FAMILY != 1\n",
        "#  error \"invalid MySQL client-family contract\"\n",
        "# elif KU_MYSQL_EXPECT_HEADER_FAMILY != KU_MYSQL_HEADER_FAMILY_MARIADB\n",
        "#  error \"selected MySQL/MariaDB client library conflicts with the selected development header\"\n",
        "# endif\n",
        "#endif\n",
        "typedef struct KuMysqlClient KuMysqlClient;\n",
        "typedef struct KuMysqlResult KuMysqlResult;\n",
        "static KuMysqlClient* ku_move_mysql_client(KuMysqlClient** p);\n",
        "static void ku_drop_mysql_client(KuMysqlClient** p);\n",
        "static KuMysqlClient* ku_clone_mysql_client(KuMysqlClient* c);\n",
        "static KuMysqlResult* ku_move_mysql_result(KuMysqlResult** p);\n",
        "static void ku_drop_mysql_result(KuMysqlResult** p);\n",
        "static KuMysqlResult* ku_clone_mysql_result(KuMysqlResult* r);\n\n",
    ));
}

/// Emit the pooled MySQL runtime. Every SQL operation uses MYSQL_STMT:
/// parameters are never escaped into SQL text. Results are detached into
/// Ku-owned, bounded buffers before the connection is returned to the pool.
fn emit_mysql_runtime(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_mysql(program) {
        return;
    }
    out.push_str(
        r#"
#define KU_FEATURE_LIBMYSQL 1
#define KU_NATIVE_RUNTIME_MYSQL 1

#define KU_MYSQL_MAX_CONNECTIONS 256U
#define KU_MYSQL_MAX_WAITERS 4096U
#define KU_MYSQL_MAX_TIMEOUT_MS 300000U
#define KU_MYSQL_MAX_CONFIG_BYTES 65536ULL
#define KU_MYSQL_MAX_SQL_BYTES (16ULL * 1024ULL * 1024ULL)
#define KU_MYSQL_MAX_PARAMS 65535U
#define KU_MYSQL_MAX_PARAM_BYTES (64ULL * 1024ULL * 1024ULL)
#define KU_MYSQL_MAX_COLUMNS 256U
#define KU_MYSQL_MAX_ROWS 1000000ULL
#define KU_MYSQL_MAX_CELLS 4194304ULL
#define KU_MYSQL_MAX_CELL_BYTES (16ULL * 1024ULL * 1024ULL)
#define KU_MYSQL_MAX_RESULT_BYTES (64ULL * 1024ULL * 1024ULL)
#define KU_MYSQL_FETCH_BUFFER_BYTES 256U

/* The fault hook is compiled only by the deterministic fake-client harness.
   Production artifacts call the C allocator directly. */
#if defined(KU_MYSQL_TEST_ALLOCATOR)
static size_t ku_mysql_test_fail_after = SIZE_MAX;
static bool ku_mysql_test_allocation_fails(void) {
  if (ku_mysql_test_fail_after == SIZE_MAX) return false;
  if (ku_mysql_test_fail_after == 0) {
    ku_mysql_test_fail_after = SIZE_MAX;
    return true;
  }
  ku_mysql_test_fail_after--;
  return false;
}
static void ku_mysql_test_fail_allocation_after(size_t successful_allocations) {
  ku_mysql_test_fail_after = successful_allocations;
}
static void* ku_mysql_malloc(size_t bytes) {
  return ku_mysql_test_allocation_fails() ? NULL : malloc(bytes);
}
static void* ku_mysql_calloc(size_t count, size_t bytes) {
  return ku_mysql_test_allocation_fails() ? NULL : calloc(count, bytes);
}
static void* ku_mysql_realloc(void* old, size_t bytes) {
  return ku_mysql_test_allocation_fails() ? NULL : realloc(old, bytes);
}
#else
#define ku_mysql_malloc(bytes) malloc(bytes)
#define ku_mysql_calloc(count, bytes) calloc((count), (bytes))
#define ku_mysql_realloc(old, bytes) realloc((old), (bytes))
#endif
#define ku_mysql_free(value) free(value)

#if defined(MARIADB_BASE_VERSION) \
    || !defined(MYSQL_VERSION_ID) || MYSQL_VERSION_ID < 80000
typedef my_bool KuMysqlBool;
#else
typedef bool KuMysqlBool;
#endif

typedef struct {
  MYSQL* connection;
  bool busy;
} KuMysqlSlot;

#if defined(_WIN32)
typedef CRITICAL_SECTION KuMysqlMutex;
typedef CONDITION_VARIABLE KuMysqlCondition;
#else
typedef pthread_mutex_t KuMysqlMutex;
typedef pthread_cond_t KuMysqlCondition;
#endif

struct KuMysqlClient {
  char* host;
  char* user;
  char* password;
  size_t password_len;
  char* database;
  unsigned int port;
  unsigned int max_connections;
  unsigned int max_waiters;
  unsigned int connect_timeout_ms;
  unsigned int acquire_timeout_ms;
  unsigned int query_timeout_ms;
  KuMysqlSlot* slots;
  KuMysqlMutex mutex;
  KuMysqlCondition condition;
  size_t active;
  size_t waiters;
  uint32_t consecutive_connect_failures;
  unsigned long long reconnect_not_before_ms;
  bool closing;
  bool finalizing;
  bool sync_ready;
  bool connect_in_flight;
  bool backoff_timer_armed;
};

typedef struct {
  size_t offset;
  size_t len;
  bool is_null;
} KuMysqlCell;

struct KuMysqlResult {
  size_t rows;
  size_t cols;
  size_t cell_count;
  size_t cell_capacity;
  KuMysqlCell* cells;
  uint8_t* data;
  size_t data_len;
  size_t data_capacity;
};

#define KU_MYSQL_LIBRARY_UNINITIALIZED 0
#define KU_MYSQL_LIBRARY_READY 1
#define KU_MYSQL_LIBRARY_FAILED (-1)
#define KU_MYSQL_LIBRARY_ABI_MISMATCH (-2)

/* Parse only a bounded leading major from a vendor version string. */
static bool ku_mysql_client_info_major(
    const char* info, unsigned long* output) {
  if (!info || !output) return false;
  unsigned long major = 0;
  for (size_t index = 0; index < 20; index++) {
    unsigned char byte = (unsigned char)info[index];
    if (byte == '.') {
      if (index == 0) return false;
      *output = major;
      return true;
    }
    if (byte < '0' || byte > '9') return false;
    unsigned long digit = (unsigned long)(byte - '0');
    if (major > (ULONG_MAX - digit) / 10UL) return false;
    major = major * 10UL + digit;
  }
  return false;
}

static bool ku_mysql_client_abi_compatible(void) {
  /* The caller has successfully completed mysql_library_init(). */
  /* MariaDB server-bundled Connector/C builds have changed whether the legacy
     mysql_get_client_* APIs report the server package or Connector/C package
     version. The generic MariaDB API accepts a NULL connection handle for
     MARIADB_CLIENT_VERSION[_ID], so use only that API for this family and
     compare it with the matching header compatibility version. Connector/C
     package support remains a separate compile-time gate above. */
#if KU_MYSQL_HEADER_FAMILY_MARIADB
  size_t runtime_version = 0;
  const char* runtime_info = NULL;
  unsigned long info_major = 0;
  if (mariadb_get_infov(
          NULL, MARIADB_CLIENT_VERSION_ID, &runtime_version) != 0
      || mariadb_get_infov(
          NULL, MARIADB_CLIENT_VERSION, &runtime_info) != 0
      || runtime_version > (size_t)ULONG_MAX
      || !ku_mysql_client_info_major(runtime_info, &info_major)) return false;
  unsigned long runtime_major =
      (unsigned long)runtime_version / 10000UL;
  return runtime_major == info_major
      && runtime_major == (unsigned long)(MARIADB_VERSION_ID / 10000UL);
#else
  /* Oracle MySQL documents these as the runtime client-library version. */
  unsigned long runtime_version = mysql_get_client_version();
  const char* runtime_info = mysql_get_client_info();
  unsigned long info_major = 0;
  if (!ku_mysql_client_info_major(runtime_info, &info_major)) return false;
  unsigned long runtime_major = runtime_version / 10000UL;
  if (runtime_major != info_major
      || runtime_major != (unsigned long)KU_MYSQL_HEADER_ABI_MAJOR) return false;
  return runtime_major >= 5UL;
#endif
}

static int ku_mysql_library_status = 0;

static void ku_mysql_library_shutdown(void) {
  mysql_library_end();
}

#if defined(_WIN32)
static INIT_ONCE ku_mysql_library_once = INIT_ONCE_STATIC_INIT;
static BOOL CALLBACK ku_mysql_library_initialize_once(
    PINIT_ONCE once, PVOID parameter, PVOID* context) {
  (void)once;
  (void)parameter;
  (void)context;
  if (mysql_library_init(0, NULL, NULL) != 0) {
    ku_mysql_library_status = KU_MYSQL_LIBRARY_FAILED;
  } else if (!ku_mysql_client_abi_compatible()) {
    mysql_library_end();
    ku_mysql_library_status = KU_MYSQL_LIBRARY_ABI_MISMATCH;
  } else if (atexit(ku_mysql_library_shutdown) != 0) {
    mysql_library_end();
    ku_mysql_library_status = KU_MYSQL_LIBRARY_FAILED;
  } else {
    ku_mysql_library_status = KU_MYSQL_LIBRARY_READY;
  }
  return TRUE;
}

static bool ku_mysql_library_ready(void) {
  if (!InitOnceExecuteOnce(
          &ku_mysql_library_once, ku_mysql_library_initialize_once,
          NULL, NULL)) return false;
  return ku_mysql_library_status == KU_MYSQL_LIBRARY_READY;
}
#else
static pthread_once_t ku_mysql_library_once = PTHREAD_ONCE_INIT;
static void ku_mysql_library_initialize_once(void) {
  if (mysql_library_init(0, NULL, NULL) != 0) {
    ku_mysql_library_status = KU_MYSQL_LIBRARY_FAILED;
  } else if (!ku_mysql_client_abi_compatible()) {
    mysql_library_end();
    ku_mysql_library_status = KU_MYSQL_LIBRARY_ABI_MISMATCH;
  } else if (atexit(ku_mysql_library_shutdown) != 0) {
    mysql_library_end();
    ku_mysql_library_status = KU_MYSQL_LIBRARY_FAILED;
  } else {
    ku_mysql_library_status = KU_MYSQL_LIBRARY_READY;
  }
}

static bool ku_mysql_library_ready(void) {
  return pthread_once(&ku_mysql_library_once, ku_mysql_library_initialize_once) == 0
      && ku_mysql_library_status == KU_MYSQL_LIBRARY_READY;
}
#endif

static KU_THREAD_LOCAL unsigned int ku_mysql_thread_depth = 0;
static KU_THREAD_LOCAL bool ku_mysql_thread_initialized = false;

static bool ku_mysql_thread_enter(void) {
  if (!ku_mysql_library_ready() || ku_mysql_thread_depth == UINT_MAX) return false;
  if (!ku_mysql_thread_initialized) {
    if (mysql_thread_init() != 0) return false;
    ku_mysql_thread_initialized = true;
  }
  ku_mysql_thread_depth++;
  return true;
}

static void ku_mysql_thread_leave(void) {
  if (ku_mysql_thread_depth == 0) {
    fputs("mysql client thread state is unbalanced\n", stderr);
    exit(1);
  }
  ku_mysql_thread_depth--;
}

/* Ku's main thread and HTTP workers retain one initialized libmysql thread
   context for their lifetime. This makes owned-handle destruction allocation
   free; the wrapper/worker calls shutdown only after all Ku values are gone. */
static void ku_mysql_thread_shutdown(void) {
  if (ku_mysql_thread_depth != 0) {
    fputs("mysql client thread state is still in use during shutdown\n", stderr);
    exit(1);
  }
  if (ku_mysql_thread_initialized) {
    mysql_thread_end();
    ku_mysql_thread_initialized = false;
  }
}

static KuError ku_mysql_error(const char* code, const char* message) {
  return ku_error_make(
      ku_string_static((const uint8_t*)"mysql", sizeof("mysql") - 1),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}

static KuError ku_mysql_execution_unknown_error(void) {
  return ku_mysql_error(
      "execution_unknown",
      "MySQL statement may have executed; outcome is unknown; never retry automatically");
}

static KuError ku_mysql_execution_completed_without_result_error(void) {
  return ku_mysql_error(
      "execution_completed_without_result",
      "MySQL statement completed but its result could not be delivered; never retry automatically");
}

/* Return 1 for valid UTF-8, 0 for invalid UTF-8 and -1 when an optional
   absolute deadline expires. Query input uses the deadline-aware path;
   configuration/result validation passes zero because it has a separate,
   already-bounded lifecycle. */
static int ku_mysql_utf8_valid_until(
    const uint8_t* data, size_t len, unsigned long long deadline) {
  size_t index = 0;
  size_t next_deadline_check = 0;
  while (index < len) {
    if (deadline != 0 && index >= next_deadline_check) {
      if (__ku_handler_now_ms() >= deadline) return -1;
      next_deadline_check = len - index > 4096 ? index + 4096 : len;
    }
    uint8_t first = data[index++];
    if (first <= 0x7f) continue;
    uint32_t scalar = 0;
    size_t remaining = 0;
    if (first >= 0xc2 && first <= 0xdf) {
      scalar = (uint32_t)(first & 0x1f);
      remaining = 1;
    } else if (first >= 0xe0 && first <= 0xef) {
      scalar = (uint32_t)(first & 0x0f);
      remaining = 2;
    } else if (first >= 0xf0 && first <= 0xf4) {
      scalar = (uint32_t)(first & 0x07);
      remaining = 3;
    } else {
      return 0;
    }
    if (remaining > len - index) return 0;
    for (size_t part = 0; part < remaining; part++) {
      uint8_t next = data[index++];
      if ((next & 0xc0) != 0x80) return 0;
      scalar = (scalar << 6) | (uint32_t)(next & 0x3f);
    }
    if ((remaining == 2 && scalar < 0x800)
        || (remaining == 3 && scalar < 0x10000)
        || scalar > 0x10ffff
        || (scalar >= 0xd800 && scalar <= 0xdfff)) return 0;
  }
  if (deadline != 0 && __ku_handler_now_ms() >= deadline) return -1;
  return 1;
}

static int ku_mysql_utf8_valid(const uint8_t* data, size_t len) {
  return ku_mysql_utf8_valid_until(data, len, 0) == 1;
}

static int ku_mysql_string_has_nul_until(
    KuString value, unsigned long long deadline) {
  if (value.len && !value.ptr) return 1;
  for (size_t offset = 0; offset < value.len;) {
    if (deadline != 0 && __ku_handler_now_ms() >= deadline) return -1;
    size_t part = value.len - offset;
    if (part > 4096) part = 4096;
    if (memchr(value.ptr + offset, 0, part)) return 1;
    offset += part;
  }
  return deadline != 0 && __ku_handler_now_ms() >= deadline ? -1 : 0;
}

static bool ku_mysql_sync_init(KuMysqlClient* client) {
#if defined(_WIN32)
  InitializeCriticalSection(&client->mutex);
  InitializeConditionVariable(&client->condition);
  return true;
#else
  if (pthread_mutex_init(&client->mutex, NULL) != 0) return false;
#if defined(__APPLE__)
  int condition_status = pthread_cond_init(&client->condition, NULL);
#else
  pthread_condattr_t attributes;
  int condition_status = pthread_condattr_init(&attributes);
  if (condition_status == 0) {
    condition_status = pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
    if (condition_status == 0) {
      condition_status = pthread_cond_init(&client->condition, &attributes);
    }
    pthread_condattr_destroy(&attributes);
  }
#endif
  if (condition_status != 0) {
    pthread_mutex_destroy(&client->mutex);
    return false;
  }
  return true;
#endif
}

static void ku_mysql_sync_destroy(KuMysqlClient* client) {
#if defined(_WIN32)
  DeleteCriticalSection(&client->mutex);
#else
  if (pthread_cond_destroy(&client->condition) != 0) {
    fputs("mysql client condition destroy failed\n", stderr);
    exit(1);
  }
  if (pthread_mutex_destroy(&client->mutex) != 0) {
    fputs("mysql client mutex destroy failed\n", stderr);
    exit(1);
  }
#endif
}

static void ku_mysql_lock(KuMysqlClient* client) {
#if defined(_WIN32)
  EnterCriticalSection(&client->mutex);
#else
  if (pthread_mutex_lock(&client->mutex) != 0) {
    fputs("mysql client mutex lock failed\n", stderr);
    exit(1);
  }
#endif
}

static void ku_mysql_unlock(KuMysqlClient* client) {
#if defined(_WIN32)
  LeaveCriticalSection(&client->mutex);
#else
  if (pthread_mutex_unlock(&client->mutex) != 0) {
    fputs("mysql client mutex unlock failed\n", stderr);
    exit(1);
  }
#endif
}

static void ku_mysql_wake_all(KuMysqlClient* client) {
#if defined(_WIN32)
  WakeAllConditionVariable(&client->condition);
#else
  if (pthread_cond_broadcast(&client->condition) != 0) {
    fputs("mysql client condition broadcast failed\n", stderr);
    exit(1);
  }
#endif
}

static void ku_mysql_wake_one(KuMysqlClient* client) {
#if defined(_WIN32)
  WakeConditionVariable(&client->condition);
#else
  if (pthread_cond_signal(&client->condition) != 0) {
    fputs("mysql client condition signal failed\n", stderr);
    exit(1);
  }
#endif
}

/* 1=woken, 0=deadline timeout, -1=synchronization failure. */
static int ku_mysql_wait_until(KuMysqlClient* client, unsigned long long deadline) {
  unsigned long long now = __ku_handler_now_ms();
  if (now >= deadline) return false;
  unsigned long long remaining = deadline - now;
#if defined(_WIN32)
  DWORD wait_ms = remaining > 0xfffffffeULL ? 0xfffffffeUL : (DWORD)remaining;
  if (SleepConditionVariableCS(&client->condition, &client->mutex, wait_ms)) return 1;
  return GetLastError() == ERROR_TIMEOUT ? 0 : -1;
#else
#if defined(__APPLE__)
  struct timespec relative = {
    (time_t)(remaining / 1000ULL),
    (long)((remaining % 1000ULL) * 1000000ULL)
  };
  int wait_status = pthread_cond_timedwait_relative_np(
      &client->condition, &client->mutex, &relative);
#else
  struct timespec absolute = {0};
  if (clock_gettime(CLOCK_MONOTONIC, &absolute) != 0) return -1;
  absolute.tv_sec += (time_t)(remaining / 1000ULL);
  long extra_nanos = (long)((remaining % 1000ULL) * 1000000ULL);
  if (absolute.tv_nsec > 999999999L - extra_nanos) {
    absolute.tv_sec++;
    absolute.tv_nsec -= 1000000000L - extra_nanos;
  } else {
    absolute.tv_nsec += extra_nanos;
  }
  int wait_status = pthread_cond_timedwait(
      &client->condition, &client->mutex, &absolute);
#endif
  if (wait_status == 0) return 1;
  return wait_status == ETIMEDOUT ? 0 : -1;
#endif
}

static unsigned long long ku_mysql_deadline(unsigned int timeout_ms) {
  unsigned long long now = __ku_handler_now_ms();
  unsigned long long deadline =
      ~0ULL - now < (unsigned long long)timeout_ms
          ? ~0ULL
          : now + (unsigned long long)timeout_ms;
  if (__ku_handler_deadline != 0 && __ku_handler_deadline < deadline) {
    deadline = __ku_handler_deadline;
  }
  return deadline;
}

static unsigned long long ku_mysql_saturating_add_ms(
    unsigned long long now, unsigned long long delay) {
  return ~0ULL - now < delay ? ~0ULL : now + delay;
}

static unsigned long long ku_mysql_backoff_delay_ms(
    KuMysqlClient* client, unsigned long long now) {
  uint32_t failures = client->consecutive_connect_failures;
  unsigned int shift = failures > 6U ? 6U : (failures ? failures - 1U : 0U);
  unsigned long long window = 25ULL << shift;
  if (window > 1000ULL) window = 1000ULL;
  unsigned long long mixed = (unsigned long long)(uintptr_t)client
      ^ now ^ ((unsigned long long)failures * 0x9e3779b97f4a7c15ULL);
  mixed ^= mixed >> 30;
  mixed *= 0xbf58476d1ce4e5b9ULL;
  mixed ^= mixed >> 27;
  mixed *= 0x94d049bb133111ebULL;
  mixed ^= mixed >> 31;
  unsigned long long lower = (window + 1ULL) / 2ULL;
  return lower + mixed % (window - lower + 1ULL);
}

static void ku_mysql_record_connect_failure_locked(
    KuMysqlClient* client, unsigned long long now) {
  if (client->consecutive_connect_failures != UINT32_MAX) {
    client->consecutive_connect_failures++;
  }
  client->reconnect_not_before_ms = ku_mysql_saturating_add_ms(
      now, ku_mysql_backoff_delay_ms(client, now));
}

static void ku_mysql_record_connect_success_locked(KuMysqlClient* client) {
  client->consecutive_connect_failures = 0;
  client->reconnect_not_before_ms = 0;
}

static bool ku_mysql_key_is(KuString key, const char* expected) {
  size_t len = strlen(expected);
  return key.len == len && (len == 0 || memcmp(key.ptr, expected, len) == 0);
}

static bool ku_mysql_config_key_known(KuString key) {
  static const char* keys[] = {
    "host", "port", "user", "password", "database",
    "max_connections", "max_waiters", "connect_timeout_ms",
    "acquire_timeout_ms", "query_timeout_ms"
  };
  for (size_t index = 0; index < sizeof(keys) / sizeof(keys[0]); index++) {
    if (ku_mysql_key_is(key, keys[index])) return true;
  }
  return false;
}

static KuValue* ku_mysql_config_value(KuObject* config, const char* key) {
  return ku_object_get(
      config, ku_string_static((const uint8_t*)key, strlen(key)));
}

static bool ku_mysql_config_string(
    KuObject* config, const char* key, bool allow_empty, KuString* output,
    KuError* error) {
  KuValue* value = ku_mysql_config_value(config, key);
  if (!value || value->tag != KU_STR) {
    *error = ku_mysql_error(
        "invalid_config", "MySQL client config is missing a required string field");
    return false;
  }
  if ((!allow_empty && value->as.s.len == 0)
      || value->as.s.len > KU_MYSQL_MAX_CONFIG_BYTES
      || (value->as.s.len && !value->as.s.ptr)
      || (value->as.s.len && memchr(value->as.s.ptr, 0, value->as.s.len) != NULL)
      || !ku_mysql_utf8_valid(value->as.s.ptr, value->as.s.len)) {
    *error = ku_mysql_error(
        "invalid_config", "MySQL client config contains an invalid string field");
    return false;
  }
  *output = value->as.s;
  return true;
}

static bool ku_mysql_config_uint(
    KuObject* config, const char* key, unsigned int fallback,
    unsigned int minimum, unsigned int maximum, unsigned int* output,
    KuError* error) {
  KuValue* value = ku_mysql_config_value(config, key);
  if (!value) {
    *output = fallback;
    return true;
  }
  if (value->tag != KU_INT || value->as.i < (int64_t)minimum
      || value->as.i > (int64_t)maximum) {
    *error = ku_mysql_error(
        "invalid_config", "MySQL client config integer is outside its allowed range");
    return false;
  }
  *output = (unsigned int)value->as.i;
  return true;
}

static char* ku_mysql_config_copy(KuString value) {
  if (value.len == SIZE_MAX) return NULL;
  char* output = (char*)ku_mysql_malloc(value.len + 1);
  if (!output) return NULL;
  if (value.len) memcpy(output, value.ptr, value.len);
  output[value.len] = 0;
  return output;
}

static void ku_mysql_result_free(KuMysqlResult* result) {
  if (!result) return;
  ku_mysql_free(result->cells);
  ku_mysql_free(result->data);
  ku_mysql_free(result);
}

static KuMysqlResult* ku_move_mysql_result(KuMysqlResult** value) {
  KuMysqlResult* moved = value ? *value : NULL;
  if (value) *value = NULL;
  return moved;
}

static void ku_drop_mysql_result(KuMysqlResult** value) {
  if (value && *value) {
    ku_mysql_result_free(*value);
    *value = NULL;
  }
}

static KuMysqlResult* ku_clone_mysql_result(KuMysqlResult* result) {
  (void)result;
  fputs("cannot clone a mysql result\n", stderr);
  exit(1);
}

static bool ku_mysql_result_reserve_cells(
    KuMysqlResult* result, size_t required, KuError* error) {
  if (required > KU_MYSQL_MAX_CELLS) {
    *error = ku_mysql_error("result_too_large", "MySQL result has too many cells");
    return false;
  }
  if (required <= result->cell_capacity) return true;
  size_t capacity = result->cell_capacity ? result->cell_capacity : 64;
  while (capacity < required) {
    if (capacity >= KU_MYSQL_MAX_CELLS / 2) {
      capacity = KU_MYSQL_MAX_CELLS;
      break;
    }
    capacity *= 2;
  }
  if (capacity > SIZE_MAX / sizeof(KuMysqlCell)) {
    *error = ku_mysql_error("result_too_large", "MySQL result has too many cells");
    return false;
  }
  KuMysqlCell* cells =
      (KuMysqlCell*)ku_mysql_realloc(
          result->cells, capacity * sizeof(KuMysqlCell));
  if (!cells) {
    *error = ku_mysql_error("out_of_memory", "MySQL result allocation failed");
    return false;
  }
  result->cells = cells;
  result->cell_capacity = capacity;
  return true;
}

static bool ku_mysql_result_append(
    KuMysqlResult* result, const uint8_t* data, size_t len, bool is_null,
    KuError* error) {
  if (!ku_mysql_result_reserve_cells(result, result->cell_count + 1, error)) {
    return false;
  }
  size_t offset = result->data_len;
  if (!is_null && len) {
    if (len > KU_MYSQL_MAX_CELL_BYTES
        || result->data_len > KU_MYSQL_MAX_RESULT_BYTES - len) {
      *error = ku_mysql_error(
          "result_too_large", "MySQL result text exceeds its memory limit");
      return false;
    }
    size_t required = result->data_len + len;
    if (required > result->data_capacity) {
      size_t capacity = result->data_capacity ? result->data_capacity : 4096;
      while (capacity < required) {
        if (capacity >= KU_MYSQL_MAX_RESULT_BYTES / 2) {
          capacity = KU_MYSQL_MAX_RESULT_BYTES;
          break;
        }
        capacity *= 2;
      }
      uint8_t* resized = (uint8_t*)ku_mysql_realloc(result->data, capacity);
      if (!resized) {
        *error = ku_mysql_error("out_of_memory", "MySQL result allocation failed");
        return false;
      }
      result->data = resized;
      result->data_capacity = capacity;
    }
    memcpy(result->data + result->data_len, data, len);
    result->data_len += len;
  }
  result->cells[result->cell_count++] =
      (KuMysqlCell){ offset, is_null ? 0 : len, is_null };
  return true;
}

static void ku_mysql_secure_wipe(char* value, size_t len) {
  if (!value) return;
  volatile unsigned char* bytes = (volatile unsigned char*)value;
  while (len) bytes[--len] = 0;
}

static void ku_mysql_secure_free(char* value, size_t len) {
  if (!value) return;
  ku_mysql_secure_wipe(value, len);
  ku_mysql_free(value);
}

static void ku_mysql_client_free_fields(KuMysqlClient* client) {
  if (!client) return;
  ku_mysql_free(client->host);
  ku_mysql_free(client->user);
  ku_mysql_secure_free(client->password, client->password_len);
  ku_mysql_free(client->database);
  ku_mysql_free(client->slots);
  client->host = NULL;
  client->user = NULL;
  client->password = NULL;
  client->password_len = 0;
  client->database = NULL;
  client->slots = NULL;
}

static void ku_mysql_close_connections(KuMysqlClient* client) {
  if (!client || !client->slots) return;
  for (size_t index = 0; index < client->max_connections; index++) {
    if (client->slots[index].connection) {
      mysql_close(client->slots[index].connection);
      client->slots[index].connection = NULL;
    }
  }
}

static KuMysqlClient* ku_move_mysql_client(KuMysqlClient** value) {
  KuMysqlClient* moved = value ? *value : NULL;
  if (value) *value = NULL;
  return moved;
}

static void ku_mysql_client_destroy(KuMysqlClient* client) {
  if (!client) return;
  bool has_connection = false;
  if (client->slots) {
    for (size_t index = 0; index < client->max_connections; index++) {
      if (client->slots[index].connection) {
        has_connection = true;
        break;
      }
    }
  }
  if (has_connection && !ku_mysql_thread_enter()) {
    /* A connection can only reach an uninitialized thread when an external C
       caller violates Ku's move-only owner/thread contract. Never kill the
       process from a destructor. Retain the still-owned allocation instead of
       invoking undefined libmysql cleanup on that unsupported path. */
    fputs("mysql client cleanup rejected an uninitialized foreign thread\n", stderr);
    return;
  } else {
    ku_mysql_close_connections(client);
    if (has_connection) ku_mysql_thread_leave();
  }
  /* A POSIX synchronization destructor is fail-stop. Scrub the still-owned
     password before it can terminate the process, but keep the allocation live
     until the embedded mutex/condition have been destroyed. */
  ku_mysql_secure_wipe(client->password, client->password_len);
  if (client->sync_ready) ku_mysql_sync_destroy(client);
  ku_mysql_client_free_fields(client);
  ku_mysql_free(client);
}

/* Called with the mutex held. Exactly one closer/borrower/waiter receives true
   and performs destruction after unlocking. */
static bool ku_mysql_finalize_ready(KuMysqlClient* client) {
  if (!client->closing || client->finalizing
      || client->active != 0 || client->waiters != 0) return false;
  client->finalizing = true;
  return true;
}

static uint8_t ku_mysql_client_close(KuMysqlClient* client) {
  if (!client) return 0;
  if (!client->sync_ready) {
    ku_mysql_client_destroy(client);
    return 0;
  }
  ku_mysql_lock(client);
  client->closing = true;
  ku_mysql_wake_all(client);
  bool finalize = ku_mysql_finalize_ready(client);
  ku_mysql_unlock(client);
  if (finalize) ku_mysql_client_destroy(client);
  return 0;
}

static void ku_drop_mysql_client(KuMysqlClient** value) {
  if (value && *value) {
    ku_mysql_client_close(*value);
    *value = NULL;
  }
}

static KuMysqlClient* ku_clone_mysql_client(KuMysqlClient* client) {
  (void)client;
  fputs("cannot clone a mysql client\n", stderr);
  exit(1);
}

static unsigned int ku_mysql_remaining_ms(
    unsigned long long deadline, unsigned int configured) {
  unsigned long long now = __ku_handler_now_ms();
  if (now >= deadline) return 0;
  unsigned long long remaining = deadline - now;
  return remaining < configured ? (unsigned int)remaining : configured;
}

static MYSQL* ku_mysql_open_connection(
    KuMysqlClient* client, KuError* error,
    unsigned long long connect_deadline,
    unsigned long long operation_deadline) {
  unsigned int connect_ms = ku_mysql_remaining_ms(
      connect_deadline, client->connect_timeout_ms);
  unsigned int query_ms = client->query_timeout_ms;
  if (!connect_ms || __ku_handler_now_ms() >= operation_deadline) {
    *error = ku_mysql_error(
        "connect_timeout", "MySQL connection budget expired");
    return NULL;
  }
  MYSQL* connection = mysql_init(NULL);
  if (!connection) {
    *error = ku_mysql_error("out_of_memory", "MySQL connection allocation failed");
    return NULL;
  }
  unsigned int connect_seconds = (connect_ms + 999U) / 1000U;
  unsigned int query_seconds = (query_ms + 999U) / 1000U;
  unsigned int local_infile = 0;
  /* Supported Oracle and MariaDB clients initialize automatic reconnect off.
     Do not call deprecated MYSQL_OPT_RECONNECT even to write false: current
     Oracle clients warn to stderr for any use of that option. Ku never enables
     reconnect and treats a broken pooled connection as contaminated instead. */
  if (mysql_options(connection, MYSQL_OPT_CONNECT_TIMEOUT, &connect_seconds) != 0
      || mysql_options(connection, MYSQL_OPT_READ_TIMEOUT, &query_seconds) != 0
      || mysql_options(connection, MYSQL_OPT_WRITE_TIMEOUT, &query_seconds) != 0
      || mysql_options(connection, MYSQL_OPT_LOCAL_INFILE, &local_infile) != 0
      || mysql_options(connection, MYSQL_SET_CHARSET_NAME, "utf8mb4") != 0) {
    mysql_close(connection);
    *error = ku_mysql_error(
        "connect_error", "MySQL connection options were rejected");
    return NULL;
  }
  MYSQL* connected = mysql_real_connect(
      connection, client->host, client->user, client->password,
      client->database, client->port, NULL, 0);
  if (__ku_handler_now_ms() >= connect_deadline) {
    mysql_close(connection);
    *error = ku_mysql_error(
        "connect_timeout", "MySQL connection budget expired");
    return NULL;
  }
  if (!connected) {
    mysql_close(connection);
    *error = ku_mysql_error("connect_error", "MySQL connection failed");
    return NULL;
  }
  int charset_status = mysql_set_character_set(connection, "utf8mb4");
  if (__ku_handler_now_ms() >= connect_deadline) {
    mysql_close(connection);
    *error = ku_mysql_error(
        "connect_timeout", "MySQL connection budget expired");
    return NULL;
  }
  if (charset_status != 0) {
    mysql_close(connection);
    *error = ku_mysql_error(
        "connect_error", "MySQL server rejected the utf8mb4 character set");
    return NULL;
  }
  return connection;
}

static KuResult_mysql_client ku_mysql_client_new(KuObject* config) {
  KuError error = (KuError){0};
  if (!config || !config->entries || config->cap == 0) {
    return (KuResult_mysql_client){
      false, NULL,
      ku_mysql_error("invalid_config", "mysql.client expects a config object")
    };
  }
  for (size_t index = 0; index < config->cap; index++) {
    if (config->entries[index].used
        && !ku_mysql_config_key_known(config->entries[index].key)) {
      return (KuResult_mysql_client){
        false, NULL,
        ku_mysql_error("invalid_config", "MySQL client config has an unknown field")
      };
    }
  }

  KuString host = {0}, user = {0}, password = {0}, database = {0};
  if (!ku_mysql_config_string(config, "host", false, &host, &error)
      || !ku_mysql_config_string(config, "user", false, &user, &error)
      || !ku_mysql_config_string(config, "password", true, &password, &error)
      || !ku_mysql_config_string(config, "database", false, &database, &error)) {
    return (KuResult_mysql_client){ false, NULL, error };
  }

  unsigned int port = 0;
  unsigned int max_connections = 0;
  unsigned int max_waiters = 0;
  unsigned int connect_timeout_ms = 0;
  unsigned int acquire_timeout_ms = 0;
  unsigned int query_timeout_ms = 0;
  /* Reject the complete config before even version-probing or initializing the
     client library. Invalid user input must have no external client side effect. */
  if (!ku_mysql_config_uint(
          config, "port", 3306, 1, 65535, &port, &error)
      || !ku_mysql_config_uint(
          config, "max_connections", 8, 1, KU_MYSQL_MAX_CONNECTIONS,
          &max_connections, &error)
      || !ku_mysql_config_uint(
          config, "max_waiters", 64, 0, KU_MYSQL_MAX_WAITERS,
          &max_waiters, &error)
      || !ku_mysql_config_uint(
          config, "connect_timeout_ms", 5000, 1, KU_MYSQL_MAX_TIMEOUT_MS,
          &connect_timeout_ms, &error)
      || !ku_mysql_config_uint(
          config, "acquire_timeout_ms", 5000, 1, KU_MYSQL_MAX_TIMEOUT_MS,
          &acquire_timeout_ms, &error)
      || !ku_mysql_config_uint(
          config, "query_timeout_ms", 30000, 1, KU_MYSQL_MAX_TIMEOUT_MS,
          &query_timeout_ms, &error)) {
    return (KuResult_mysql_client){ false, NULL, error };
  }

  if (!ku_mysql_library_ready()) {
    if (ku_mysql_library_status == KU_MYSQL_LIBRARY_ABI_MISMATCH) {
      return (KuResult_mysql_client){
        false, NULL,
        ku_mysql_error(
            "client_abi_mismatch",
            "MySQL client headers and runtime library are ABI-incompatible")
      };
    }
    return (KuResult_mysql_client){
      false, NULL,
      ku_mysql_error("sync_error", "MySQL client library initialization failed")
    };
  }

  KuMysqlClient* client =
      (KuMysqlClient*)ku_mysql_calloc(1, sizeof(KuMysqlClient));
  if (!client) {
    return (KuResult_mysql_client){
      false, NULL,
      ku_mysql_error("out_of_memory", "MySQL client allocation failed")
    };
  }
  client->port = port;
  client->max_connections = max_connections;
  client->max_waiters = max_waiters;
  client->connect_timeout_ms = connect_timeout_ms;
  client->acquire_timeout_ms = acquire_timeout_ms;
  client->query_timeout_ms = query_timeout_ms;

  client->host = ku_mysql_config_copy(host);
  client->user = ku_mysql_config_copy(user);
  client->password = ku_mysql_config_copy(password);
  client->password_len = password.len;
  client->database = ku_mysql_config_copy(database);
  client->slots = (KuMysqlSlot*)ku_mysql_calloc(
      client->max_connections, sizeof(KuMysqlSlot));
  if (!client->host || !client->user || !client->password
      || !client->database || !client->slots) {
    ku_mysql_client_free_fields(client);
    ku_mysql_free(client);
    return (KuResult_mysql_client){
      false, NULL,
      ku_mysql_error("out_of_memory", "MySQL client allocation failed")
    };
  }
  if (!ku_mysql_sync_init(client)) {
    ku_mysql_client_free_fields(client);
    ku_mysql_free(client);
    return (KuResult_mysql_client){
      false, NULL,
      ku_mysql_error("sync_error", "MySQL client synchronization failed")
    };
  }
  client->sync_ready = true;

  if (!ku_mysql_thread_enter()) {
    ku_mysql_client_close(client);
    return (KuResult_mysql_client){
      false, NULL,
      ku_mysql_error("out_of_memory", "MySQL thread state allocation failed")
    };
  }
  unsigned long long initial_connect_deadline =
      ku_mysql_deadline(client->connect_timeout_ms);
  unsigned long long initial_operation_deadline =
      ku_mysql_deadline(client->query_timeout_ms);
  MYSQL* first = ku_mysql_open_connection(
      client, &error, initial_connect_deadline, initial_operation_deadline);
  ku_mysql_thread_leave();
  if (!first) {
    ku_mysql_client_close(client);
    return (KuResult_mysql_client){ false, NULL, error };
  }
  client->slots[0].connection = first;
  return (KuResult_mysql_client){ true, client, (KuError){0} };
}

/* Called with the client lock held. A timed-out waiter must hand an available
   slot to another queued waiter instead of consuming the release signal. */
static bool ku_mysql_slot_available_locked(KuMysqlClient* client) {
  for (size_t index = 0; index < client->max_connections; index++) {
    if (client->slots[index].connection && !client->slots[index].busy) return true;
  }
  if (client->connect_in_flight
      || __ku_handler_now_ms() < client->reconnect_not_before_ms) return false;
  for (size_t index = 0; index < client->max_connections; index++) {
    if (!client->slots[index].connection && !client->slots[index].busy) return true;
  }
  return false;
}

static void ku_mysql_handoff_available_locked(KuMysqlClient* client) {
  if (client->waiters != 0 && ku_mysql_slot_available_locked(client)) {
    ku_mysql_wake_one(client);
  }
}

/* A timed waiter owns the reconnect wake-up only while it is blocked. If it
   wakes for another reason or its own deadline wins, hand that responsibility
   to another queued waiter before this thread can leave the acquire loop. */
static void ku_mysql_release_backoff_timer_locked(
    KuMysqlClient* client, bool owned) {
  if (!owned) return;
  client->backoff_timer_armed = false;
  if (client->waiters > 1) ku_mysql_wake_one(client);
}

static MYSQL* ku_mysql_acquire(
    KuMysqlClient* client, size_t* slot_index, KuError* error,
    unsigned long long operation_deadline) {
  if (!client || !slot_index) {
    *error = ku_mysql_error("client_closed", "MySQL client is closed");
    return NULL;
  }
  unsigned long long deadline = ku_mysql_deadline(client->acquire_timeout_ms);
  if (operation_deadline < deadline) deadline = operation_deadline;
  bool registered_waiter = false;
  ku_mysql_lock(client);
  for (;;) {
    if (client->closing) {
      if (registered_waiter) client->waiters--;
      ku_mysql_wake_all(client);
      bool finalize = ku_mysql_finalize_ready(client);
      ku_mysql_unlock(client);
      if (finalize) ku_mysql_client_destroy(client);
      *error = ku_mysql_error("client_closed", "MySQL client is closed");
      return NULL;
    }
    if (__ku_handler_now_ms() >= deadline) {
      if (registered_waiter) {
        client->waiters--;
        ku_mysql_handoff_available_locked(client);
      }
      bool finalize = ku_mysql_finalize_ready(client);
      ku_mysql_unlock(client);
      if (finalize) ku_mysql_client_destroy(client);
      *error = ku_mysql_error(
          "acquire_timeout", "Timed out waiting for a MySQL connection");
      return NULL;
    }
    bool can_claim = registered_waiter || client->waiters == 0;
    if (can_claim) {
      for (size_t index = 0; index < client->max_connections; index++) {
        if (client->slots[index].connection && !client->slots[index].busy) {
          client->slots[index].busy = true;
          client->active++;
          if (registered_waiter) client->waiters--;
          *slot_index = index;
          MYSQL* connection = client->slots[index].connection;
          ku_mysql_unlock(client);
          return connection;
        }
      }
      bool can_connect = !client->connect_in_flight
          && __ku_handler_now_ms() >= client->reconnect_not_before_ms;
      for (size_t index = 0; can_connect && index < client->max_connections; index++) {
        if (!client->slots[index].connection && !client->slots[index].busy) {
          client->slots[index].busy = true;
          client->active++;
          client->connect_in_flight = true;
          if (registered_waiter) client->waiters--;
          *slot_index = index;
          unsigned long long connect_budget_deadline =
              ku_mysql_saturating_add_ms(
                  __ku_handler_now_ms(), client->connect_timeout_ms);
          bool acquire_limited_connect = deadline <= connect_budget_deadline;
          unsigned long long connect_deadline = acquire_limited_connect
              ? deadline : connect_budget_deadline;
          ku_mysql_unlock(client);
          MYSQL* connection = ku_mysql_open_connection(
              client, error, connect_deadline, operation_deadline);
          if (!connection) {
            if (acquire_limited_connect
                && ku_mysql_key_is(error->code, "connect_timeout")) {
              ku_error_drop(error);
              *error = ku_mysql_error(
                  "acquire_timeout", "Timed out acquiring a MySQL connection");
            }
            ku_mysql_lock(client);
            client->connect_in_flight = false;
            ku_mysql_record_connect_failure_locked(
                client, __ku_handler_now_ms());
            client->slots[index].busy = false;
            client->active--;
            if (client->closing) ku_mysql_wake_all(client);
            else ku_mysql_wake_one(client);
            bool finalize = ku_mysql_finalize_ready(client);
            ku_mysql_unlock(client);
            if (finalize) ku_mysql_client_destroy(client);
            return NULL;
          }
          ku_mysql_lock(client);
          client->connect_in_flight = false;
          ku_mysql_record_connect_success_locked(client);
          bool closed = client->closing;
          bool expired = __ku_handler_now_ms() >= deadline;
          if (closed || expired) {
            client->slots[index].busy = false;
            client->active--;
            if (closed) ku_mysql_wake_all(client);
            else ku_mysql_handoff_available_locked(client);
            bool finalize = ku_mysql_finalize_ready(client);
            ku_mysql_unlock(client);
            mysql_close(connection);
            if (finalize) ku_mysql_client_destroy(client);
            *error = closed
                ? ku_mysql_error("client_closed", "MySQL client is closed")
                : ku_mysql_error(
                    "acquire_timeout", "Timed out acquiring a MySQL connection");
            return NULL;
          }
          client->slots[index].connection = connection;
          ku_mysql_wake_one(client);
          ku_mysql_unlock(client);
          return connection;
        }
      }
    }
    if (!registered_waiter) {
      if (client->waiters >= client->max_waiters) {
        ku_mysql_unlock(client);
        *error = ku_mysql_error("pool_busy", "MySQL client waiter limit reached");
        return NULL;
      }
      client->waiters++;
      registered_waiter = true;
    }
    unsigned long long wait_deadline = deadline;
    bool owns_backoff_timer = false;
    unsigned long long now = __ku_handler_now_ms();
    if (client->reconnect_not_before_ms > now
        && !client->backoff_timer_armed) {
      client->backoff_timer_armed = true;
      owns_backoff_timer = true;
      if (client->reconnect_not_before_ms < wait_deadline) {
        wait_deadline = client->reconnect_not_before_ms;
      }
    }
    int wait_status = ku_mysql_wait_until(client, wait_deadline);
    ku_mysql_release_backoff_timer_locked(client, owns_backoff_timer);
    if (wait_status <= 0) {
      now = __ku_handler_now_ms();
      if (wait_status == 0 && wait_deadline < deadline && now < deadline) {
        continue;
      }
      client->waiters--;
      if (!client->closing) ku_mysql_handoff_available_locked(client);
      bool finalize = ku_mysql_finalize_ready(client);
      ku_mysql_unlock(client);
      if (finalize) ku_mysql_client_destroy(client);
      *error = wait_status == 0
          ? ku_mysql_error(
              "acquire_timeout", "Timed out waiting for a MySQL connection")
          : ku_mysql_error(
              "sync_error", "MySQL connection wait failed");
      return NULL;
    }
  }
}

static bool ku_mysql_reset_for_pool(
    MYSQL* connection, bool broken, unsigned long long deadline);

static void ku_mysql_release(
    KuMysqlClient* client, size_t slot_index, bool broken,
    unsigned long long deadline) {
  ku_mysql_lock(client);
  bool reset_allowed = !client->closing;
  MYSQL* connection = slot_index < client->max_connections
      ? client->slots[slot_index].connection
      : NULL;
  ku_mysql_unlock(client);
  if (!reset_allowed) broken = true;
  else broken = ku_mysql_reset_for_pool(connection, broken, deadline);
  ku_mysql_lock(client);
  MYSQL* discard = NULL;
  if (slot_index < client->max_connections) {
    KuMysqlSlot* slot = &client->slots[slot_index];
    if (broken) {
      discard = slot->connection;
      slot->connection = NULL;
    }
    slot->busy = false;
  }
  if (client->active) client->active--;
  if (client->closing) ku_mysql_wake_all(client);
  else ku_mysql_wake_one(client);
  bool finalize = ku_mysql_finalize_ready(client);
  ku_mysql_unlock(client);
  if (discard) mysql_close(discard);
  if (finalize) ku_mysql_client_destroy(client);
}

static bool ku_mysql_connection_error(unsigned int code) {
#if defined(CR_SERVER_GONE_ERROR)
  if (code == CR_SERVER_GONE_ERROR) return true;
#endif
#if defined(CR_SERVER_LOST)
  if (code == CR_SERVER_LOST) return true;
#endif
#if defined(CR_SERVER_LOST_EXTENDED)
  if (code == CR_SERVER_LOST_EXTENDED) return true;
#endif
#if defined(CR_COMMANDS_OUT_OF_SYNC)
  if (code == CR_COMMANDS_OUT_OF_SYNC) return true;
#endif
  return code == 2006U || code == 2013U || code == 2055U || code == 2014U;
}

static bool ku_mysql_execute_outcome_unknown(unsigned int code) {
#if defined(CR_MIN_ERROR) && defined(CR_MAX_ERROR)
  if (code >= (unsigned int)CR_MIN_ERROR && code <= (unsigned int)CR_MAX_ERROR) {
    return true;
  }
#endif
#if defined(CER_MIN_ERROR) && defined(CER_MAX_ERROR)
  if (code >= (unsigned int)CER_MIN_ERROR && code <= (unsigned int)CER_MAX_ERROR) {
    return true;
  }
#endif
  return ku_mysql_connection_error(code);
}

static void ku_mysql_statement_close_checked(
    MYSQL_STMT** statement, bool free_result, bool* broken) {
  if (!statement || !*statement) return;
  MYSQL_STMT* owned = *statement;
  *statement = NULL;
  if (free_result && mysql_stmt_free_result(owned) != 0) *broken = true;
  if (mysql_stmt_close(owned) != 0) *broken = true;
}

static bool ku_mysql_reset_for_pool(
    MYSQL* connection, bool broken, unsigned long long deadline) {
  if (!connection || broken || __ku_handler_now_ms() >= deadline) return true;
  int reset_status = mysql_reset_connection(connection);
  if (reset_status != 0 || __ku_handler_now_ms() >= deadline) return true;
  int charset_status = mysql_set_character_set(connection, "utf8mb4");
  return charset_status != 0 || __ku_handler_now_ms() >= deadline;
}

static bool ku_mysql_session_state_is_supported(MYSQL* connection) {
  /* These protocol status bits are shared by Oracle MySQL and MariaDB. The
     matching public mysql.h is required because MYSQL is a versioned ABI. */
  const unsigned int in_transaction = 1U;
  const unsigned int autocommit = 2U;
  return connection
      && (connection->server_status & in_transaction) == 0
      && (connection->server_status & autocommit) != 0;
}

static bool ku_mysql_sql_keyword_byte(uint8_t value) {
  return (value >= (uint8_t)'A' && value <= (uint8_t)'Z')
      || (value >= (uint8_t)'a' && value <= (uint8_t)'z')
      || (value >= (uint8_t)'0' && value <= (uint8_t)'9')
      || value == (uint8_t)'_';
}

static bool ku_mysql_sql_token_equals(
    KuString sql, size_t start, size_t len, const char* expected) {
  size_t expected_len = strlen(expected);
  if (len != expected_len) return false;
  for (size_t index = 0; index < len; index++) {
    uint8_t value = sql.ptr[start + index];
    if (value >= (uint8_t)'A' && value <= (uint8_t)'Z') value += 32;
    if (value != (uint8_t)expected[index]) return false;
  }
  return true;
}

/* Conservatively reject executable-comment markers anywhere in statement
   text, including quoted text. That small false-positive surface is preferable
   to attempting a partial emulation of server/version-specific comment rules. */
static int ku_mysql_sql_contains_executable_comment(
    KuString sql, unsigned long long deadline) {
  size_t next_deadline_check = 0;
  for (size_t index = 0; index + 2 < sql.len; index++) {
    if (index >= next_deadline_check) {
      if (__ku_handler_now_ms() >= deadline) return -1;
      next_deadline_check = SIZE_MAX - index < 4096
          ? SIZE_MAX : index + 4096;
    }
    if (sql.ptr[index] != (uint8_t)'/'
        || sql.ptr[index + 1] != (uint8_t)'*') continue;
    if (sql.ptr[index + 2] == (uint8_t)'!') return 1;
    if (index + 3 < sql.len
        && (sql.ptr[index + 2] == (uint8_t)'M'
            || sql.ptr[index + 2] == (uint8_t)'m')
        && sql.ptr[index + 3] == (uint8_t)'!') return 1;
  }
  return __ku_handler_now_ms() >= deadline ? -1 : 0;
}

/* MySQL has two executable block-comment forms (`/*!` and `/*M!`) and `#`
   line comments. The ordinary pooled client rejects executable comments
   rather than trying to interpret version gates embedded in them. Return 1
   for a token, 0 for end/non-keyword input, -1 for fail-closed syntax, and -2
   when the already-established operation deadline expires. */
static int ku_mysql_sql_next_top_token(
    KuString sql, size_t* cursor, size_t* start, size_t* len,
    unsigned long long deadline) {
  size_t index = *cursor;
  size_t next_deadline_check = index;
  for (;;) {
#define KU_MYSQL_SQL_SCAN_CHECK() do { \
  if (index >= next_deadline_check) { \
    if (__ku_handler_now_ms() >= deadline) return -2; \
    next_deadline_check = SIZE_MAX - index < 4096 ? SIZE_MAX : index + 4096; \
  } \
} while (0)
    while (index < sql.len) {
      KU_MYSQL_SQL_SCAN_CHECK();
      uint8_t value = sql.ptr[index];
      if (value != (uint8_t)' ' && value != (uint8_t)'\t'
          && value != (uint8_t)'\r' && value != (uint8_t)'\n'
          && value != (uint8_t)'\f' && value != (uint8_t)'\v') break;
      index++;
    }
    if (index == 0 && sql.len >= 3 && sql.ptr[0] == 0xef
        && sql.ptr[1] == 0xbb && sql.ptr[2] == 0xbf) {
      index = 3;
      continue;
    }
    if (index + 1 < sql.len && sql.ptr[index] == (uint8_t)'-'
        && sql.ptr[index + 1] == (uint8_t)'-'
        && index + 2 < sql.len && sql.ptr[index + 2] <= (uint8_t)' ') {
      index += 2;
      while (index < sql.len && sql.ptr[index] != (uint8_t)'\n'
          && sql.ptr[index] != (uint8_t)'\r') {
        KU_MYSQL_SQL_SCAN_CHECK();
        index++;
      }
      continue;
    }
    if (index < sql.len && sql.ptr[index] == (uint8_t)'#') {
      index++;
      while (index < sql.len && sql.ptr[index] != (uint8_t)'\n'
          && sql.ptr[index] != (uint8_t)'\r') {
        KU_MYSQL_SQL_SCAN_CHECK();
        index++;
      }
      continue;
    }
    if (index + 1 < sql.len && sql.ptr[index] == (uint8_t)'/'
        && sql.ptr[index + 1] == (uint8_t)'*') {
      bool executable = index + 2 < sql.len && sql.ptr[index + 2] == (uint8_t)'!';
      bool mariadb_executable = index + 3 < sql.len
          && (sql.ptr[index + 2] == (uint8_t)'M'
              || sql.ptr[index + 2] == (uint8_t)'m')
          && sql.ptr[index + 3] == (uint8_t)'!';
      if (executable || mariadb_executable) return -1;
      /* Unlike PostgreSQL, MySQL/MariaDB ordinary block comments do not
         nest. Stop at the first closing delimiter so a later top-level SET
         cannot be hidden from this scanner by a fake nested opener. */
      bool closed = false;
      index += 2;
      while (index < sql.len) {
        KU_MYSQL_SQL_SCAN_CHECK();
        if (index + 1 < sql.len && sql.ptr[index] == (uint8_t)'*'
            && sql.ptr[index + 1] == (uint8_t)'/') {
          index += 2;
          closed = true;
          break;
        }
        index++;
      }
      if (!closed) return -1;
      continue;
    }
    break;
  }
  KU_MYSQL_SQL_SCAN_CHECK();
  if (index >= sql.len) { *cursor = index; return 0; }
  if (sql.ptr[index] == (uint8_t)';') return -1;
  if (!ku_mysql_sql_keyword_byte(sql.ptr[index])
      || (sql.ptr[index] >= (uint8_t)'0' && sql.ptr[index] <= (uint8_t)'9')) {
    *cursor = index;
    return 0;
  }
  *start = index;
  while (index < sql.len && ku_mysql_sql_keyword_byte(sql.ptr[index])) {
    KU_MYSQL_SQL_SCAN_CHECK();
    index++;
  }
  *len = index - *start; *cursor = index;
#undef KU_MYSQL_SQL_SCAN_CHECK
  return 1;
}

/* This is deliberately a narrow policy guard, not a proof of session purity:
   SQL can still invoke stored functions or vendor extensions. Protocol state
   is checked again after execution, and every reusable connection is reset. */
static int ku_mysql_sql_has_explicit_session_control(
    KuString sql, unsigned long long deadline) {
  size_t cursor = 0, start = 0, len = 0;
  int token = ku_mysql_sql_next_top_token(
      sql, &cursor, &start, &len, deadline);
  if (token <= 0) return token;
  static const char* const forbidden[] = {
    "begin", "start", "commit", "rollback", "savepoint", "release",
    "set", "reset", "lock", "unlock", "use", "xa", "prepare",
    "execute", "deallocate", "handler", "flush", "call"
  };
  for (size_t index = 0;
       index < sizeof(forbidden) / sizeof(forbidden[0]); index++) {
    if (ku_mysql_sql_token_equals(sql, start, len, forbidden[index])) return 1;
  }
  bool create_statement = ku_mysql_sql_token_equals(sql, start, len, "create");
  bool drop_statement = ku_mysql_sql_token_equals(sql, start, len, "drop");
  if (create_statement || drop_statement) {
    token = ku_mysql_sql_next_top_token(
        sql, &cursor, &start, &len, deadline);
    if (token <= 0) return token;
    if (create_statement && ku_mysql_sql_token_equals(sql, start, len, "or")) {
      token = ku_mysql_sql_next_top_token(
          sql, &cursor, &start, &len, deadline);
      if (token <= 0) return token;
      if (!ku_mysql_sql_token_equals(sql, start, len, "replace")) return 0;
      token = ku_mysql_sql_next_top_token(
          sql, &cursor, &start, &len, deadline);
      if (token <= 0) return token;
    }
    if (ku_mysql_sql_token_equals(sql, start, len, "temporary")) return 1;
  }
  return 0;
}

static KuError ku_mysql_session_state_error(void) {
  return ku_mysql_error(
      "session_state_unsupported",
      "MySQL statement was not sent because its SQL is unsupported by the pooled client session-safety policy");
}

static KuError ku_mysql_post_execution_session_state_error(void) {
  return ku_mysql_error(
      "session_state_unsupported",
      "MySQL statement completed or may have completed; session state is unsupported and its payload was discarded; never retry automatically");
}

static bool ku_mysql_validate_statement_input(
    KuString sql, KuArray_str params, KuError* error,
    unsigned long long deadline) {
  if (__ku_handler_now_ms() >= deadline) {
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return false;
  }
  if (sql.len && !sql.ptr) {
    *error = ku_mysql_error("query_error", "MySQL SQL storage is invalid");
    return false;
  }
  if (sql.len > KU_MYSQL_MAX_SQL_BYTES) {
    *error = ku_mysql_error("query_too_large", "MySQL SQL text exceeds its limit");
    return false;
  }
  int has_nul = ku_mysql_string_has_nul_until(sql, deadline);
  if (has_nul < 0) {
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return false;
  }
  if (has_nul > 0) {
    *error = ku_mysql_error("query_error", "MySQL SQL text contains a NUL byte");
    return false;
  }
  int valid_sql = ku_mysql_utf8_valid_until(sql.ptr, sql.len, deadline);
  if (valid_sql < 0) {
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return false;
  }
  if (valid_sql == 0) {
    *error = ku_mysql_error("invalid_utf8", "MySQL SQL text is not valid UTF-8");
    return false;
  }
  int executable_comment =
      ku_mysql_sql_contains_executable_comment(sql, deadline);
  if (executable_comment < 0) {
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return false;
  }
  if (executable_comment > 0) {
    *error = ku_mysql_session_state_error();
    return false;
  }
  int session_control =
      ku_mysql_sql_has_explicit_session_control(sql, deadline);
  if (session_control == -2) {
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return false;
  }
  if (session_control != 0) {
    *error = ku_mysql_session_state_error();
    return false;
  }
  if (params.len > KU_MYSQL_MAX_PARAMS || (params.len && !params.data)) {
    *error = ku_mysql_error(
        "parameter_too_large", "MySQL parameters exceed their count limit");
    return false;
  }
  size_t total = 0;
  for (size_t index = 0; index < params.len; index++) {
    if (__ku_handler_now_ms() >= deadline) {
      *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
      return false;
    }
    KuString value = params.data[index];
    if ((value.len && !value.ptr)
        || value.len > KU_MYSQL_MAX_CELL_BYTES
        || value.len > (size_t)ULONG_MAX
        || total > KU_MYSQL_MAX_PARAM_BYTES - value.len) {
      *error = ku_mysql_error(
          "parameter_too_large", "MySQL parameter data exceeds its limit");
      return false;
    }
    int valid_param = ku_mysql_utf8_valid_until(value.ptr, value.len, deadline);
    if (valid_param < 0) {
      *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
      return false;
    }
    if (valid_param == 0) {
      *error = ku_mysql_error(
          "invalid_utf8", "MySQL parameter text is not valid UTF-8");
      return false;
    }
    total += value.len;
  }
  if (__ku_handler_now_ms() >= deadline) {
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return false;
  }
  return true;
}

static MYSQL_STMT* ku_mysql_prepare_and_execute(
    MYSQL* connection, KuString sql, KuArray_str params,
    KuError* error, bool* broken, unsigned long long deadline) {
  if (__ku_handler_now_ms() >= deadline) {
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return NULL;
  }
  MYSQL_STMT* statement = mysql_stmt_init(connection);
  if (!statement) {
    *error = ku_mysql_error("out_of_memory", "MySQL statement allocation failed");
    return NULL;
  }
  int prepare_status = mysql_stmt_prepare(
      statement, (const char*)sql.ptr, (unsigned long)sql.len);
  if (__ku_handler_now_ms() >= deadline) {
    *broken = true;
    ku_mysql_statement_close_checked(&statement, false, broken);
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return NULL;
  }
  if (prepare_status != 0) {
    unsigned int code = mysql_stmt_errno(statement);
    *broken = ku_mysql_connection_error(code);
    ku_mysql_statement_close_checked(&statement, false, broken);
    *error = ku_mysql_error("query_error", "MySQL statement prepare failed");
    return NULL;
  }
  unsigned long expected = mysql_stmt_param_count(statement);
  if ((size_t)expected != params.len) {
    ku_mysql_statement_close_checked(&statement, false, broken);
    *error = ku_mysql_error(
        "parameter_count", "MySQL placeholder and parameter counts differ");
    return NULL;
  }

  MYSQL_BIND* bindings = NULL;
  unsigned long* lengths = NULL;
  if (params.len) {
    bindings =
        (MYSQL_BIND*)ku_mysql_calloc(params.len, sizeof(MYSQL_BIND));
    lengths =
        (unsigned long*)ku_mysql_calloc(params.len, sizeof(unsigned long));
    if (!bindings || !lengths) {
      ku_mysql_free(bindings);
      ku_mysql_free(lengths);
      ku_mysql_statement_close_checked(&statement, false, broken);
      *error = ku_mysql_error("out_of_memory", "MySQL parameter allocation failed");
      return NULL;
    }
    for (size_t index = 0; index < params.len; index++) {
      lengths[index] = (unsigned long)params.data[index].len;
      bindings[index].buffer_type = MYSQL_TYPE_STRING;
      bindings[index].buffer =
          params.data[index].len ? (void*)params.data[index].ptr : (void*)"";
      bindings[index].buffer_length = lengths[index];
      bindings[index].length = &lengths[index];
      bindings[index].is_null = NULL;
    }
    if (mysql_stmt_bind_param(statement, bindings) != 0) {
      unsigned int code = mysql_stmt_errno(statement);
      *broken = ku_mysql_connection_error(code);
      ku_mysql_free(bindings);
      ku_mysql_free(lengths);
      ku_mysql_statement_close_checked(&statement, false, broken);
      *error = ku_mysql_error("query_error", "MySQL parameter binding failed");
      return NULL;
    }
  }
  if (__ku_handler_now_ms() >= deadline) {
    ku_mysql_free(bindings);
    ku_mysql_free(lengths);
    *broken = true;
    ku_mysql_statement_close_checked(&statement, false, broken);
    *error = ku_mysql_error("query_timeout", "MySQL query budget expired");
    return NULL;
  }
  int execute_status = mysql_stmt_execute(statement);
  ku_mysql_free(bindings);
  ku_mysql_free(lengths);
  if (execute_status != 0) {
    unsigned int code = mysql_stmt_errno(statement);
    bool outcome_unknown = ku_mysql_execute_outcome_unknown(code);
    *broken = outcome_unknown;
    ku_mysql_statement_close_checked(&statement, true, broken);
    *error = outcome_unknown
        ? ku_mysql_execution_unknown_error()
        : ku_mysql_error("query_error", "MySQL statement execution failed");
    return NULL;
  }
  if (__ku_handler_now_ms() >= deadline) {
    unsigned int column_count = mysql_stmt_field_count(statement);
    *broken = true;
    ku_mysql_statement_close_checked(&statement, true, broken);
    *error = column_count
        ? ku_mysql_execution_unknown_error()
        : ku_mysql_execution_completed_without_result_error();
    return NULL;
  }
  return statement;
}

static bool ku_mysql_binary_field(const MYSQL_FIELD* field) {
  if (!field || field->charsetnr != 63U) return false;
  switch (field->type) {
    case MYSQL_TYPE_BIT:
    case MYSQL_TYPE_STRING:
    case MYSQL_TYPE_VAR_STRING:
    case MYSQL_TYPE_BLOB:
    case MYSQL_TYPE_TINY_BLOB:
    case MYSQL_TYPE_MEDIUM_BLOB:
    case MYSQL_TYPE_LONG_BLOB:
    case MYSQL_TYPE_GEOMETRY:
      return true;
    default:
      return false;
  }
}

static KuMysqlResult* ku_mysql_fetch_result(
    MYSQL_STMT* statement, KuError* error, bool* broken,
    unsigned long long deadline) {
  unsigned int column_count = mysql_stmt_field_count(statement);
  if (__ku_handler_now_ms() >= deadline) {
    *broken = true;
    *error = column_count
        ? ku_mysql_execution_unknown_error()
        : ku_mysql_execution_completed_without_result_error();
    return NULL;
  }
  MYSQL_RES* metadata = mysql_stmt_result_metadata(statement);
  if (column_count && !metadata) {
    unsigned int code = mysql_stmt_errno(statement);
    *broken = true;
    (void)code;
    *error = ku_mysql_execution_unknown_error();
    return NULL;
  }
  if (column_count > KU_MYSQL_MAX_COLUMNS) {
    if (metadata) mysql_free_result(metadata);
    *broken = true;
    *error = ku_mysql_execution_unknown_error();
    return NULL;
  }
  if (metadata) {
    MYSQL_FIELD* fields = mysql_fetch_fields(metadata);
    if (!fields && column_count) {
      mysql_free_result(metadata);
      *broken = true;
      *error = ku_mysql_execution_unknown_error();
      return NULL;
    }
    for (unsigned int index = 0; index < column_count; index++) {
      if (ku_mysql_binary_field(&fields[index])) {
        mysql_free_result(metadata);
        *broken = true;
        *error = ku_mysql_execution_unknown_error();
        return NULL;
      }
    }
    mysql_free_result(metadata);
  }

  KuMysqlResult* result =
      (KuMysqlResult*)ku_mysql_calloc(1, sizeof(KuMysqlResult));
  if (!result) {
    *broken = true;
    *error = column_count
        ? ku_mysql_execution_unknown_error()
        : ku_mysql_execution_completed_without_result_error();
    return NULL;
  }
  result->cols = column_count;
  if (!column_count) return result;

  MYSQL_BIND* bindings =
      (MYSQL_BIND*)ku_mysql_calloc(column_count, sizeof(MYSQL_BIND));
  unsigned long* lengths =
      (unsigned long*)ku_mysql_calloc(column_count, sizeof(unsigned long));
  KuMysqlBool* nulls =
      (KuMysqlBool*)ku_mysql_calloc(column_count, sizeof(KuMysqlBool));
  KuMysqlBool* errors =
      (KuMysqlBool*)ku_mysql_calloc(column_count, sizeof(KuMysqlBool));
  uint8_t* buffers = (uint8_t*)ku_mysql_malloc(
      (size_t)column_count * KU_MYSQL_FETCH_BUFFER_BYTES);
  if (!bindings || !lengths || !nulls || !errors || !buffers) {
    ku_mysql_free(bindings);
    ku_mysql_free(lengths);
    ku_mysql_free(nulls);
    ku_mysql_free(errors);
    ku_mysql_free(buffers);
    ku_mysql_result_free(result);
    *broken = true;
    *error = ku_mysql_execution_unknown_error();
    return NULL;
  }
  for (unsigned int index = 0; index < column_count; index++) {
    bindings[index].buffer_type = MYSQL_TYPE_STRING;
    bindings[index].buffer =
        buffers + (size_t)index * KU_MYSQL_FETCH_BUFFER_BYTES;
    bindings[index].buffer_length = KU_MYSQL_FETCH_BUFFER_BYTES;
    bindings[index].length = &lengths[index];
    bindings[index].is_null = &nulls[index];
    bindings[index].error = &errors[index];
  }
  if (mysql_stmt_bind_result(statement, bindings) != 0) {
    unsigned int code = mysql_stmt_errno(statement);
    *broken = true;
    (void)code;
    ku_mysql_free(bindings);
    ku_mysql_free(lengths);
    ku_mysql_free(nulls);
    ku_mysql_free(errors);
    ku_mysql_free(buffers);
    ku_mysql_result_free(result);
    *error = ku_mysql_execution_unknown_error();
    return NULL;
  }

  bool failed = false;
  bool fully_read = false;
  for (;;) {
    if (__ku_handler_now_ms() >= deadline) {
      *broken = true;
      *error = ku_mysql_execution_unknown_error();
      failed = true;
      break;
    }
    memset(lengths, 0, (size_t)column_count * sizeof(unsigned long));
    memset(nulls, 0, (size_t)column_count * sizeof(KuMysqlBool));
    memset(errors, 0, (size_t)column_count * sizeof(KuMysqlBool));
    int status = mysql_stmt_fetch(statement);
    if (status == MYSQL_NO_DATA) {
      fully_read = true;
      if (__ku_handler_now_ms() >= deadline) {
        *broken = true;
        *error = ku_mysql_execution_completed_without_result_error();
        failed = true;
      }
      break;
    }
    if (__ku_handler_now_ms() >= deadline) {
      *broken = true;
      *error = ku_mysql_execution_unknown_error();
      failed = true;
      break;
    }
    if (status != 0 && status != MYSQL_DATA_TRUNCATED) {
      unsigned int code = mysql_stmt_errno(statement);
      *broken = true;
      (void)code;
      *error = ku_mysql_execution_unknown_error();
      failed = true;
      break;
    }
    if (result->rows >= KU_MYSQL_MAX_ROWS
        || result->cell_count > KU_MYSQL_MAX_CELLS - column_count) {
      *error = ku_mysql_execution_unknown_error();
      failed = true;
      break;
    }
    for (unsigned int index = 0; index < column_count; index++) {
      if (nulls[index]) {
        if (!ku_mysql_result_append(result, NULL, 0, true, error)) {
          ku_error_drop(error);
          *error = ku_mysql_execution_unknown_error();
          failed = true;
          break;
        }
        continue;
      }
      size_t length = (size_t)lengths[index];
      if (length > KU_MYSQL_MAX_CELL_BYTES
          || result->data_len > KU_MYSQL_MAX_RESULT_BYTES - length) {
        *error = ku_mysql_execution_unknown_error();
        failed = true;
        break;
      }
      const uint8_t* value =
          buffers + (size_t)index * KU_MYSQL_FETCH_BUFFER_BYTES;
      uint8_t* overflow = NULL;
      if (errors[index] || length > KU_MYSQL_FETCH_BUFFER_BYTES) {
        overflow = (uint8_t*)ku_mysql_malloc(length ? length : 1);
        if (!overflow) {
          *error = ku_mysql_execution_unknown_error();
          failed = true;
          break;
        }
        MYSQL_BIND column = (MYSQL_BIND){0};
        unsigned long fetched_length = (unsigned long)length;
        KuMysqlBool fetched_null = 0;
        KuMysqlBool fetched_error = 0;
        column.buffer_type = MYSQL_TYPE_STRING;
        column.buffer = overflow;
        column.buffer_length = fetched_length;
        column.length = &fetched_length;
        column.is_null = &fetched_null;
        column.error = &fetched_error;
        int column_status =
            mysql_stmt_fetch_column(statement, &column, index, 0);
        if (__ku_handler_now_ms() >= deadline) {
          ku_mysql_free(overflow);
          *broken = true;
          *error = ku_mysql_execution_unknown_error();
          failed = true;
          break;
        }
        if (column_status != 0 || fetched_null || fetched_error
            || (size_t)fetched_length != length) {
          ku_mysql_free(overflow);
          unsigned int code = mysql_stmt_errno(statement);
          *broken = ku_mysql_connection_error(code);
          *error = ku_mysql_execution_unknown_error();
          failed = true;
          break;
        }
        value = overflow;
      }
      if (!ku_mysql_utf8_valid(value, length)) {
        ku_mysql_free(overflow);
        *error = ku_mysql_execution_unknown_error();
        failed = true;
        break;
      }
      bool appended =
          ku_mysql_result_append(result, value, length, false, error);
      ku_mysql_free(overflow);
      if (!appended) {
        ku_error_drop(error);
        *error = ku_mysql_execution_unknown_error();
        failed = true;
        break;
      }
    }
    if (failed) break;
    result->rows++;
  }

  ku_mysql_free(bindings);
  ku_mysql_free(lengths);
  ku_mysql_free(nulls);
  ku_mysql_free(errors);
  ku_mysql_free(buffers);
  if (!fully_read) *broken = true;
  if (failed) {
    ku_mysql_result_free(result);
    return NULL;
  }
  return result;
}

static KuResult_mysql_result ku_mysql_client_query(
    KuMysqlClient* client, KuString sql, KuArray_str params) {
  KuError error = (KuError){0};
  if (!client) {
    return (KuResult_mysql_result){
      false, NULL, ku_mysql_error("client_closed", "MySQL client is closed")
    };
  }
  unsigned long long deadline = ku_mysql_deadline(client->query_timeout_ms);
  if (!ku_mysql_validate_statement_input(sql, params, &error, deadline)) {
    return (KuResult_mysql_result){ false, NULL, error };
  }
  if (!ku_mysql_thread_enter()) {
    return (KuResult_mysql_result){
      false, NULL,
      ku_mysql_error("out_of_memory", "MySQL thread state allocation failed")
    };
  }
  if (__ku_handler_now_ms() >= deadline) {
    ku_mysql_thread_leave();
    return (KuResult_mysql_result){
      false, NULL,
      ku_mysql_error("query_timeout", "MySQL query budget expired")
    };
  }
  size_t slot_index = 0;
  MYSQL* connection = ku_mysql_acquire(
      client, &slot_index, &error, deadline);
  if (!connection) {
    ku_mysql_thread_leave();
    return (KuResult_mysql_result){ false, NULL, error };
  }
  bool broken = false;
  MYSQL_STMT* statement = ku_mysql_prepare_and_execute(
      connection, sql, params, &error, &broken, deadline);
  KuMysqlResult* result = NULL;
  if (statement) {
    result = ku_mysql_fetch_result(statement, &error, &broken, deadline);
    if (!result) broken = true;
    ku_mysql_statement_close_checked(&statement, true, &broken);
  }
  if (result && !ku_mysql_session_state_is_supported(connection)) {
    ku_mysql_result_free(result);
    result = NULL;
    error = ku_mysql_post_execution_session_state_error();
    broken = true;
  }
  ku_mysql_release(client, slot_index, broken, deadline);
  ku_mysql_thread_leave();
  if (!result) return (KuResult_mysql_result){ false, NULL, error };
  return (KuResult_mysql_result){ true, result, (KuError){0} };
}

static KuResult_int ku_mysql_client_execute(
    KuMysqlClient* client, KuString sql, KuArray_str params) {
  KuError error = (KuError){0};
  if (!client) {
    return (KuResult_int){
      false, 0, ku_mysql_error("client_closed", "MySQL client is closed")
    };
  }
  unsigned long long deadline = ku_mysql_deadline(client->query_timeout_ms);
  if (!ku_mysql_validate_statement_input(sql, params, &error, deadline)) {
    return (KuResult_int){ false, 0, error };
  }
  if (!ku_mysql_thread_enter()) {
    return (KuResult_int){
      false, 0,
      ku_mysql_error("out_of_memory", "MySQL thread state allocation failed")
    };
  }
  if (__ku_handler_now_ms() >= deadline) {
    ku_mysql_thread_leave();
    return (KuResult_int){
      false, 0,
      ku_mysql_error("query_timeout", "MySQL query budget expired")
    };
  }
  size_t slot_index = 0;
  MYSQL* connection = ku_mysql_acquire(
      client, &slot_index, &error, deadline);
  if (!connection) {
    ku_mysql_thread_leave();
    return (KuResult_int){ false, 0, error };
  }
  bool broken = false;
  MYSQL_STMT* statement = ku_mysql_prepare_and_execute(
      connection, sql, params, &error, &broken, deadline);
  int64_t affected = 0;
  bool ok = statement != NULL;
  if (statement) {
    unsigned int column_count = mysql_stmt_field_count(statement);
    if (column_count != 0) {
      broken = true;
      error = ku_mysql_execution_unknown_error();
      ok = false;
    } else if (__ku_handler_now_ms() >= deadline) {
      error = ku_mysql_execution_completed_without_result_error();
      broken = true;
      ok = false;
    } else {
      my_ulonglong count = mysql_stmt_affected_rows(statement);
      if (count == (my_ulonglong)-1 || count > (my_ulonglong)INT64_MAX) {
        error = ku_mysql_execution_completed_without_result_error();
        broken = true;
        ok = false;
      } else {
        affected = (int64_t)count;
      }
    }
    ku_mysql_statement_close_checked(&statement, true, &broken);
  }
  if (ok && !ku_mysql_session_state_is_supported(connection)) {
    affected = 0;
    error = ku_mysql_post_execution_session_state_error();
    broken = true;
    ok = false;
  }
  ku_mysql_release(client, slot_index, broken, deadline);
  ku_mysql_thread_leave();
  if (!ok) return (KuResult_int){ false, 0, error };
  return (KuResult_int){ true, affected, (KuError){0} };
}

static int64_t ku_mysql_result_rows(KuMysqlResult* result) {
  return result ? (int64_t)result->rows : 0;
}

static int64_t ku_mysql_result_cols(KuMysqlResult* result) {
  return result ? (int64_t)result->cols : 0;
}

static bool ku_mysql_result_cell(
    KuMysqlResult* result, int64_t row, int64_t col,
    KuMysqlCell** output, KuError* error) {
  if (!result || row < 0 || col < 0
      || (size_t)row >= result->rows || (size_t)col >= result->cols) {
    *error = ku_mysql_error(
        "index_out_of_bounds", "MySQL result cell index is out of bounds");
    return false;
  }
  size_t index = (size_t)row * result->cols + (size_t)col;
  if (index >= result->cell_count) {
    *error = ku_mysql_error("result_error", "MySQL result storage is inconsistent");
    return false;
  }
  *output = &result->cells[index];
  return true;
}

static KuResult_str ku_mysql_result_value(
    KuMysqlResult* result, int64_t row, int64_t col) {
  KuError error = (KuError){0};
  KuMysqlCell* cell = NULL;
  if (!ku_mysql_result_cell(result, row, col, &cell, &error)) {
    return (KuResult_str){ false, (KuString){0}, error };
  }
  if (cell->is_null) {
    return (KuResult_str){
      false, (KuString){0},
      ku_mysql_error("null_value", "MySQL result cell is SQL NULL")
    };
  }
  if (cell->len == 0) {
    return (KuResult_str){ true, (KuString){0}, (KuError){0} };
  }
  uint8_t* copy = (uint8_t*)ku_mysql_malloc(cell->len);
  if (!copy) {
    return (KuResult_str){
      false, (KuString){0},
      ku_mysql_error("out_of_memory", "MySQL result value allocation failed")
    };
  }
  memcpy(copy, result->data + cell->offset, cell->len);
  return (KuResult_str){
    true, (KuString){copy, cell->len, cell->len, KU_STRING_OWNED},
    (KuError){0}
  };
}

static KuResult_bool ku_mysql_result_is_null(
    KuMysqlResult* result, int64_t row, int64_t col) {
  KuError error = (KuError){0};
  KuMysqlCell* cell = NULL;
  if (!ku_mysql_result_cell(result, row, col, &cell, &error)) {
    return (KuResult_bool){ false, false, error };
  }
  return (KuResult_bool){ true, cell->is_null, (KuError){0} };
}

"#,
    );
}

fn program_uses_native_named(program: &IrProgram, wanted: &str) -> bool {
    fn ty(t: &IrType, wanted: &str) -> bool {
        match t {
            IrType::Named(name) => name == wanted,
            IrType::Array(inner) | IrType::Result(inner) | IrType::Cell(inner) => ty(inner, wanted),
            IrType::Closure { params, ret, .. } => {
                params.iter().any(|param| ty(param, wanted)) || ty(ret, wanted)
            }
            _ => false,
        }
    }
    program.functions.iter().any(|function| {
        if ty(&function.return_type, wanted)
            || function.params.iter().any(|param| ty(&param.ty, wanted))
        {
            return true;
        }
        function.blocks.iter().any(|block| {
            let mut used = false;
            for instruction in &block.instructions {
                walk_inst_types(instruction, &mut |candidate| {
                    if ty(candidate, wanted) {
                        used = true;
                    }
                });
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                walk_expr_types(expr, &mut |candidate| {
                    if ty(candidate, wanted) {
                        used = true;
                    }
                });
            });
            used
        })
    })
}

fn program_uses_net(program: &IrProgram) -> bool {
    program_uses_native_named(program, "__ku_net_client")
}

/// Emit one process-level Winsock owner for the native transports that Ku owns
/// directly. Net and Redis share the same successful WSAStartup reference; it is
/// released once at normal process exit, never when an individual client closes.
fn emit_windows_socket_runtime(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_net(program) && !program_uses_redis(program) {
        return;
    }
    out.push_str(
        r#"
#define KU_NATIVE_RUNTIME_WINSOCK 1
#if defined(_WIN32)
enum {
  KU_WINSOCK_RUNTIME_UNINITIALIZED = 0,
  KU_WINSOCK_RUNTIME_READY = 1,
  KU_WINSOCK_RUNTIME_FAILED = 2
};
static INIT_ONCE ku_winsock_runtime_once = INIT_ONCE_STATIC_INIT;
static int ku_winsock_runtime_status = KU_WINSOCK_RUNTIME_UNINITIALIZED;
static void ku_winsock_runtime_shutdown(void) {
  if (ku_winsock_runtime_status != KU_WINSOCK_RUNTIME_READY) return;
  ku_winsock_runtime_status = KU_WINSOCK_RUNTIME_FAILED;
  (void)WSACleanup();
}
static BOOL CALLBACK ku_winsock_runtime_initialize_once(
    PINIT_ONCE once, PVOID parameter, PVOID* context) {
  (void)once;
  (void)parameter;
  (void)context;
  WSADATA data;
  if (WSAStartup(MAKEWORD(2, 2), &data) != 0) {
    ku_winsock_runtime_status = KU_WINSOCK_RUNTIME_FAILED;
    return TRUE;
  }
  if (LOBYTE(data.wVersion) != 2 || HIBYTE(data.wVersion) != 2) {
    (void)WSACleanup();
    ku_winsock_runtime_status = KU_WINSOCK_RUNTIME_FAILED;
    return TRUE;
  }
  if (atexit(ku_winsock_runtime_shutdown) != 0) {
    (void)WSACleanup();
    ku_winsock_runtime_status = KU_WINSOCK_RUNTIME_FAILED;
    return TRUE;
  }
  ku_winsock_runtime_status = KU_WINSOCK_RUNTIME_READY;
  return TRUE;
}
static int ku_winsock_runtime_startup(void) {
  if (!InitOnceExecuteOnce(
          &ku_winsock_runtime_once, ku_winsock_runtime_initialize_once,
          NULL, NULL)) return -1;
  return ku_winsock_runtime_status == KU_WINSOCK_RUNTIME_READY ? 0 : -1;
}
#else
static int ku_winsock_runtime_startup(void) { return 0; }
#endif

"#,
    );
}

fn program_uses_bytes(program: &IrProgram) -> bool {
    program_uses_net(program) || program_uses_native_named(program, "__ku_bytes")
}

fn emit_bytes_types(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_bytes(program) {
        return;
    }
    out.push_str(
        "typedef struct KuBytes {\n  uint8_t* ptr;\n  size_t len;\n  size_t capacity;\n  uint8_t storage;\n} KuBytes;\n\
         enum { KU_BYTES_STATIC = 0, KU_BYTES_OWNED = 1 };\n\
         static KuBytes ku_move_bytes(KuBytes* value);\n\
         static KuBytes ku_clone_bytes(KuBytes value);\n\
         static void ku_drop_bytes(KuBytes* value);\n\n",
    );
}

fn emit_net_types(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_net(program) {
        return;
    }
    out.push_str(concat!(
        "typedef struct KuNetClient KuNetClient;\n",
        "static KuNetClient* ku_move_net_client(KuNetClient** value);\n",
        "static KuNetClient* ku_clone_net_client(KuNetClient* value);\n",
        "static void ku_drop_net_client(KuNetClient** value);\n\n",
    ));
}

fn emit_bytes_runtime(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_bytes(program) {
        return;
    }
    out.push_str(
        r#"
#define KU_BYTES_MAX_LENGTH (64ULL * 1024ULL * 1024ULL)

static KuError ku_bytes_error(const char* code, const char* message) {
  return ku_error_make(
      ku_string_static((const uint8_t*)"bytes", 5),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}

static int ku_bytes_try_copy(const uint8_t* data, size_t len, KuBytes* out) {
  if (!out || (len != 0 && !data) || len > (size_t)KU_BYTES_MAX_LENGTH) return -1;
  *out = (KuBytes){0};
  if (len == 0) return 1;
  uint8_t* copy = (uint8_t*)malloc(len);
  if (!copy) return 0;
  memcpy(copy, data, len);
  *out = (KuBytes){ copy, len, len, KU_BYTES_OWNED };
  return 1;
}

static KuBytes ku_move_bytes(KuBytes* value) {
  if (!value) return (KuBytes){0};
  KuBytes moved = *value;
  *value = (KuBytes){0};
  return moved;
}

static KuBytes ku_clone_bytes(KuBytes value) {
  if (value.storage == KU_BYTES_STATIC) return value;
  KuBytes copy = (KuBytes){0};
  int copied = ku_bytes_try_copy(value.ptr, value.len, &copy);
  if (copied != 1) {
    fputs(copied == 0 ? "bytes clone allocation failed\n" : "invalid bytes clone source\n", stderr);
    exit(1);
  }
  return copy;
}

static void ku_drop_bytes(KuBytes* value) {
  if (!value) return;
  if (value->storage == KU_BYTES_OWNED && value->ptr) free(value->ptr);
  *value = (KuBytes){0};
}

static KuResult_bytes ku_bytes_from_str(KuString text) {
  if ((text.len != 0 && !text.ptr) || text.len > (size_t)KU_BYTES_MAX_LENGTH) {
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_bytes_error("too_large", "bytes input is invalid or exceeds 64 MiB") };
  }
  KuBytes value = (KuBytes){0};
  int copied = ku_bytes_try_copy(text.ptr, text.len, &value);
  if (copied != 1) {
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_bytes_error("out_of_memory", "bytes allocation failed") };
  }
  return (KuResult_bytes){ true, value, (KuError){0} };
}

static int ku_bytes_utf8_valid(const uint8_t* data, size_t len) {
  if (len != 0 && !data) return 0;
  size_t i = 0;
  while (i < len) {
    uint8_t c = data[i];
    if (c <= 0x7f) { i++; continue; }
    if (c >= 0xc2 && c <= 0xdf) {
      if (i + 1 >= len || (data[i + 1] & 0xc0) != 0x80) return 0;
      i += 2; continue;
    }
    if (c == 0xe0) {
      if (i + 2 >= len || data[i + 1] < 0xa0 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if ((c >= 0xe1 && c <= 0xec) || (c >= 0xee && c <= 0xef)) {
      if (i + 2 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xed) {
      if (i + 2 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x9f || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xf0) {
      if (i + 3 >= len || data[i + 1] < 0x90 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c >= 0xf1 && c <= 0xf3) {
      if (i + 3 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c == 0xf4) {
      if (i + 3 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x8f || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    return 0;
  }
  return 1;
}

static int64_t ku_bytes_len(KuBytes value) {
  if (value.len > (size_t)INT64_MAX) {
    fputs("bytes length is outside Ku int range\n", stderr);
    exit(1);
  }
  return (int64_t)value.len;
}

static KuResult_int ku_bytes_get(KuBytes value, int64_t index) {
  if (index < 0 || (uint64_t)index >= (uint64_t)value.len || !value.ptr) {
    return (KuResult_int){ false, 0,
      ku_bytes_error("index_out_of_bounds", "bytes index is out of bounds") };
  }
  return (KuResult_int){ true, (int64_t)value.ptr[(size_t)index], (KuError){0} };
}

static KuResult_str ku_bytes_to_str(KuBytes value) {
  if (!ku_bytes_utf8_valid(value.ptr, value.len)) {
    return (KuResult_str){ false, (KuString){0},
      ku_bytes_error("invalid_utf8", "bytes value is not valid UTF-8") };
  }
  if (value.len == 0) return (KuResult_str){ true, (KuString){0}, (KuError){0} };
  uint8_t* copy = (uint8_t*)malloc(value.len);
  if (!copy) {
    return (KuResult_str){ false, (KuString){0},
      ku_bytes_error("out_of_memory", "string allocation failed") };
  }
  memcpy(copy, value.ptr, value.len);
  return (KuResult_str){ true,
    (KuString){ copy, value.len, value.len, KU_STRING_OWNED }, (KuError){0} };
}
"#,
    );
    if program_uses_intrinsic(program, "bytes.from_array") {
        out.push_str(
            r#"
static KuResult_bytes ku_bytes_from_array(KuArray_int values) {
  if (values.len > (size_t)KU_BYTES_MAX_LENGTH || (values.len != 0 && !values.data)) {
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_bytes_error("too_large", "byte array is invalid or exceeds 64 MiB") };
  }
  for (size_t index = 0; index < values.len; index++) {
    if (values.data[index] < 0 || values.data[index] > 255) {
      return (KuResult_bytes){ false, (KuBytes){0},
        ku_bytes_error("invalid_byte", "byte values must be between 0 and 255") };
    }
  }
  KuBytes result = (KuBytes){0};
  if (values.len != 0) {
    result.ptr = (uint8_t*)malloc(values.len);
    if (!result.ptr) {
      return (KuResult_bytes){ false, (KuBytes){0},
        ku_bytes_error("out_of_memory", "bytes allocation failed") };
    }
    for (size_t index = 0; index < values.len; index++)
      result.ptr[index] = (uint8_t)values.data[index];
    result.len = values.len;
    result.capacity = values.len;
    result.storage = KU_BYTES_OWNED;
  }
  return (KuResult_bytes){ true, result, (KuError){0} };
}
"#,
        );
    }
    out.push('\n');
}

fn emit_net_runtime(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_net(program) {
        return;
    }
    if program_mentions_native_tls_config(program) {
        out.push_str("#define KU_FEATURE_NATIVE_TLS 1\n");
    }
    out.push_str(
        r#"
#define KU_NATIVE_RUNTIME_NET_SOCKET 1
#define KU_NET_DEFAULT_TIMEOUT_MS 5000U
#define KU_NET_MAX_TIMEOUT_MS 300000U
#define KU_NET_DEFAULT_MAX_READ_BYTES (1024U * 1024U)
#define KU_NET_MAX_READ_BYTES (16U * 1024U * 1024U)
#define KU_NET_MAX_HOST_BYTES 253U
#define KU_NET_MAX_TLS_CA_PEM_BYTES 4194304U
#define KU_NET_MAX_RESOLVED_ADDRESSES 64U
#define KU_NET_SOCKET_CHUNK 1073741824U
#define KU_NET_TLS_MAX_RECORD_BYTES 65540ULL
#define KU_NET_TLS_MAX_HANDSHAKE_CIPHERTEXT_BYTES 1048576ULL
#define KU_NET_TLS_MAX_NO_PROGRESS_CALLS 64U
#define KU_NET_TLS_MAX_HANDSHAKE_DRIVER_CALLS 1048640ULL
#define KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES 524320ULL
#define KU_NET_TLS_MAX_OPERATION_DRIVER_CALLS 524384ULL
#define KU_NET_TLS_MAX_DRAIN_STEPS 1024U

#if defined(KU_NATIVE_TLS_ENABLED)
#define KU_TLS_ABI_VERSION 1u
#define KU_TLS_STATUS_OK 0u
#define KU_TLS_STATUS_NULL_POINTER 1u
#define KU_TLS_STATUS_INVALID_ARGUMENT 2u
#define KU_TLS_STATUS_LIMIT_EXCEEDED 3u
#define KU_TLS_STATUS_INVALID_DNS_NAME 4u
#define KU_TLS_STATUS_INVALID_CA 5u
#define KU_TLS_STATUS_TLS_ERROR 6u
#define KU_TLS_STATUS_SESSION_FAILED 7u
#define KU_TLS_STATUS_TRUNCATED 8u
#define KU_TLS_STATUS_WOULD_BLOCK 9u
#define KU_TLS_STATUS_IO_ERROR 10u
#define KU_TLS_STATUS_PANIC 255u
#define KU_TLS_ROOTS_WEBPKI 0u
#define KU_TLS_ROOTS_CUSTOM_PEM 1u
#define KU_TLS_MAX_CA_PEM_BYTES 4194304u
#define KU_TLS_MAX_IO_BYTES 65536u
#define KU_TLS_MAX_SERVER_NAME_BYTES 253u
typedef struct KuTlsConfig KuTlsConfig;
typedef struct KuTlsClientSession KuTlsClientSession;
extern uint32_t ku_tls_abi_version(void);
extern uint32_t ku_tls_v1_build_id(const uint8_t**, size_t*);
extern uint32_t ku_tls_v1_config_new(uint32_t, const uint8_t*, size_t, KuTlsConfig**);
extern uint32_t ku_tls_v1_config_drop(KuTlsConfig*);
extern uint32_t ku_tls_v1_client_new(const KuTlsConfig*, const uint8_t*, size_t,
    KuTlsClientSession**);
extern uint32_t ku_tls_v1_client_drop(KuTlsClientSession*);
extern uint32_t ku_tls_v1_client_wants_read(const KuTlsClientSession*, uint32_t*);
extern uint32_t ku_tls_v1_client_wants_write(const KuTlsClientSession*, uint32_t*);
extern uint32_t ku_tls_v1_client_is_handshaking(const KuTlsClientSession*, uint32_t*);
extern uint32_t ku_tls_v1_client_peer_closed(const KuTlsClientSession*, uint32_t*);
extern uint32_t ku_tls_v1_client_feed_ciphertext(KuTlsClientSession*,
    const uint8_t*, size_t, size_t*);
extern uint32_t ku_tls_v1_client_process(KuTlsClientSession*);
extern uint32_t ku_tls_v1_client_drain_ciphertext(KuTlsClientSession*,
    uint8_t*, size_t, size_t*);
extern uint32_t ku_tls_v1_client_write_plaintext(KuTlsClientSession*,
    const uint8_t*, size_t, size_t*);
extern uint32_t ku_tls_v1_client_read_plaintext(KuTlsClientSession*,
    uint8_t*, size_t, size_t*);
extern uint32_t ku_tls_v1_client_send_close_notify(KuTlsClientSession*);
extern uint32_t ku_tls_v1_client_notify_eof(KuTlsClientSession*);
#endif

#if defined(_WIN32)
typedef SOCKET KuNetSocket;
#define KU_NET_INVALID_SOCKET INVALID_SOCKET
typedef struct { HANDLE semaphore; } KuNetGate;
typedef volatile LONG KuNetAtomicFlag;
static void ku_net_atomic_flag_init(KuNetAtomicFlag* flag) {
  (void)InterlockedExchange(flag, 0);
}
static int ku_net_atomic_flag_load(KuNetAtomicFlag* flag) {
  return InterlockedCompareExchange(flag, 0, 0) != 0;
}
static void ku_net_atomic_flag_set(KuNetAtomicFlag* flag) {
  (void)InterlockedExchange(flag, 1);
}
#else
typedef int KuNetSocket;
#define KU_NET_INVALID_SOCKET (-1)
typedef struct { pthread_mutex_t mutex; pthread_cond_t condition; int available; } KuNetGate;
typedef _Atomic int KuNetAtomicFlag;
static void ku_net_atomic_flag_init(KuNetAtomicFlag* flag) {
  atomic_init(flag, 0);
}
static int ku_net_atomic_flag_load(KuNetAtomicFlag* flag) {
  return atomic_load_explicit(flag, memory_order_acquire) != 0;
}
static void ku_net_atomic_flag_set(KuNetAtomicFlag* flag) {
  atomic_store_explicit(flag, 1, memory_order_release);
}
#endif

struct KuNetClient {
  KuNetSocket socket_value;
  KuNetGate gate;
  KuNetAtomicFlag poison_requested;
#if defined(KU_NATIVE_TLS_ENABLED)
  KuTlsClientSession* tls_session;
  uint8_t* tls_pending;
  size_t tls_pending_len;
  size_t tls_pending_offset;
#endif
  uint8_t tls_enabled;
  uint32_t read_timeout_ms;
  uint32_t write_timeout_ms;
  uint32_t max_read_bytes;
};

static KuError ku_net_error(const char* code, const char* message) {
  return ku_error_make(
      ku_string_static((const uint8_t*)"net", 3),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}

static unsigned long long ku_net_now_ms(void) { return __ku_handler_now_ms(); }

static unsigned long long ku_net_deadline_after_ms(uint32_t timeout_ms) {
  unsigned long long now = ku_net_now_ms();
  unsigned long long deadline = (~0ULL - now < (unsigned long long)timeout_ms)
      ? ~0ULL : now + (unsigned long long)timeout_ms;
  if (__ku_handler_deadline != 0 && __ku_handler_deadline < deadline)
    deadline = __ku_handler_deadline;
  return deadline;
}

static int ku_net_socket_last_error(void) {
#if defined(_WIN32)
  return WSAGetLastError();
#else
  return errno;
#endif
}

static int ku_net_socket_error_interrupted(int error) {
#if defined(_WIN32)
  return error == WSAEINTR;
#else
  return error == EINTR;
#endif
}

static int ku_net_socket_error_would_block(int error) {
#if defined(_WIN32)
  return error == WSAEWOULDBLOCK || error == WSAEINPROGRESS;
#else
  return error == EAGAIN || error == EWOULDBLOCK || error == EINPROGRESS;
#endif
}

static int ku_net_socket_error_connecting(int error) {
#if defined(_WIN32)
  return error == WSAEWOULDBLOCK || error == WSAEINPROGRESS
      || error == WSAEINVAL || error == WSAEINTR;
#else
  return error == EINPROGRESS || error == EALREADY
      || error == EWOULDBLOCK || error == EINTR;
#endif
}

static void ku_net_socket_close(KuNetSocket socket_value) {
  if (socket_value == KU_NET_INVALID_SOCKET) return;
#if defined(_WIN32)
  closesocket(socket_value);
#else
  (void)close(socket_value);
#endif
}

static int ku_net_socket_set_nonblocking(KuNetSocket socket_value) {
#if defined(_WIN32)
  u_long mode = 1UL;
  return ioctlsocket(socket_value, FIONBIO, &mode) == 0 ? 0 : -1;
#else
  int flags = fcntl(socket_value, F_GETFL, 0);
  if (flags < 0) return -1;
  return fcntl(socket_value, F_SETFL, flags | O_NONBLOCK) == 0 ? 0 : -1;
#endif
}

static int ku_net_socket_suppress_sigpipe(KuNetSocket socket_value) {
#if defined(__APPLE__)
  int enabled = 1;
  return setsockopt(socket_value, SOL_SOCKET, SO_NOSIGPIPE,
      &enabled, (socklen_t)sizeof(enabled)) == 0 ? 0 : -1;
#else
  (void)socket_value;
  return 0;
#endif
}

static int ku_net_socket_connect(KuNetSocket socket_value,
    const struct sockaddr* address, size_t address_len) {
#if defined(_WIN32)
  if (address_len > (size_t)INT_MAX) return SOCKET_ERROR;
  return connect(socket_value, address, (int)address_len);
#else
  if (address_len > (size_t)((socklen_t)-1)) { errno = EINVAL; return -1; }
  return connect(socket_value, address, (socklen_t)address_len);
#endif
}

static int ku_net_socket_pending_error(KuNetSocket socket_value, int* out_error) {
  int socket_error = 0;
#if defined(_WIN32)
  int length = (int)sizeof(socket_error);
  int rc = getsockopt(socket_value, SOL_SOCKET, SO_ERROR,
      (char*)&socket_error, &length);
#else
  socklen_t length = (socklen_t)sizeof(socket_error);
  int rc = getsockopt(socket_value, SOL_SOCKET, SO_ERROR,
      &socket_error, &length);
#endif
  if (rc != 0) return -1;
  *out_error = socket_error;
  return 0;
}

/* 1=ready (including EOF/error for recv/send to classify), 0=deadline, -1=OS error. */
static int ku_net_socket_wait(KuNetSocket socket_value, int writing,
    unsigned long long deadline) {
  for (;;) {
    unsigned long long now = ku_net_now_ms();
    if (now >= deadline) return 0;
    unsigned long long remaining = deadline - now;
#if defined(_WIN32)
    struct timeval wait;
    unsigned long long seconds = remaining / 1000ULL;
    wait.tv_sec = seconds > (unsigned long long)LONG_MAX ? LONG_MAX : (long)seconds;
    wait.tv_usec = (long)((remaining % 1000ULL) * 1000ULL);
    fd_set readable, writable, exceptional;
    FD_ZERO(&readable); FD_ZERO(&writable); FD_ZERO(&exceptional);
    if (writing) FD_SET(socket_value, &writable); else FD_SET(socket_value, &readable);
    FD_SET(socket_value, &exceptional);
    int selected = select(0, writing ? NULL : &readable,
        writing ? &writable : NULL, &exceptional, &wait);
    if (selected > 0) return 1;
#else
    int wait_ms = remaining > (unsigned long long)INT_MAX ? INT_MAX : (int)remaining;
    if (wait_ms == 0) wait_ms = 1;
    struct pollfd descriptor;
    descriptor.fd = socket_value;
    descriptor.events = writing ? POLLOUT : POLLIN;
    descriptor.revents = 0;
    int selected = poll(&descriptor, 1, wait_ms);
    if (selected > 0) {
      if (descriptor.revents & POLLNVAL) return -1;
      return 1;
    }
#endif
    if (selected == 0) return 0;
    if (!ku_net_socket_error_interrupted(ku_net_socket_last_error())) return -1;
  }
}

static int ku_net_socket_send(KuNetSocket socket_value, const uint8_t* data, size_t len) {
  size_t chunk = len > KU_NET_SOCKET_CHUNK ? KU_NET_SOCKET_CHUNK : len;
#if defined(_WIN32)
  return send(socket_value, (const char*)data, (int)chunk, 0);
#elif defined(__APPLE__)
  ssize_t sent = send(socket_value, data, chunk, 0);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#elif defined(MSG_NOSIGNAL)
  ssize_t sent = send(socket_value, data, chunk, MSG_NOSIGNAL);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#else
#error "std.net POSIX transport requires MSG_NOSIGNAL or SO_NOSIGPIPE"
#endif
}

static int ku_net_socket_recv(KuNetSocket socket_value, uint8_t* data, size_t len) {
  size_t chunk = len > KU_NET_SOCKET_CHUNK ? KU_NET_SOCKET_CHUNK : len;
#if defined(_WIN32)
  return recv(socket_value, (char*)data, (int)chunk, 0);
#else
  ssize_t received = recv(socket_value, data, chunk, 0);
  return received > (ssize_t)INT_MAX ? INT_MAX : (int)received;
#endif
}

static int ku_net_socket_startup(void) {
  return ku_winsock_runtime_startup();
}

#if defined(KU_NATIVE_TLS_ENABLED)
static const uint8_t ku_net_tls_expected_build_id[] =
    "ku-native-tls/0.1.0;abi=1;rustls=0.23.40;ring=0.17.14;"
    "webpki-roots=1.0.7;buffer=65536;handshake=1048576;"
    "record-staging=65540;resumption=disabled";
static int ku_net_tls_abi_status = 0;
#if defined(_WIN32)
static INIT_ONCE ku_net_tls_abi_once = INIT_ONCE_STATIC_INIT;
static BOOL CALLBACK ku_net_tls_check_abi_once(
    PINIT_ONCE once, PVOID parameter, PVOID* context) {
  (void)once; (void)parameter; (void)context;
#else
static pthread_once_t ku_net_tls_abi_once = PTHREAD_ONCE_INIT;
static void ku_net_tls_check_abi_once(void) {
#endif
  const uint8_t* build_id = NULL;
  size_t build_id_len = 0;
  uint32_t status = ku_tls_v1_build_id(&build_id, &build_id_len);
  ku_net_tls_abi_status = ku_tls_abi_version() == KU_TLS_ABI_VERSION
      && status == KU_TLS_STATUS_OK && build_id
      && build_id_len == sizeof(ku_net_tls_expected_build_id) - 1
      && memcmp(build_id, ku_net_tls_expected_build_id, build_id_len) == 0 ? 1 : -1;
#if defined(_WIN32)
  return TRUE;
#endif
}

static int ku_net_tls_check_abi(void) {
#if defined(_WIN32)
  if (!InitOnceExecuteOnce(&ku_net_tls_abi_once, ku_net_tls_check_abi_once,
          NULL, NULL)) return 0;
#else
  if (pthread_once(&ku_net_tls_abi_once, ku_net_tls_check_abi_once) != 0) return 0;
#endif
  return ku_net_tls_abi_status == 1;
}

static KuError ku_net_tls_status_error(uint32_t status, const char* operation) {
  (void)operation;
  if (status == KU_TLS_STATUS_TRUNCATED)
    return ku_net_error("tls_truncated", "net TLS peer closed without close_notify");
  if (status == KU_TLS_STATUS_INVALID_CA)
    return ku_net_error("invalid_config", "net TLS custom CA bundle is invalid");
  if (status == KU_TLS_STATUS_INVALID_DNS_NAME)
    return ku_net_error("invalid_config", "net TLS server name is invalid");
  if (status == KU_TLS_STATUS_LIMIT_EXCEEDED)
    return ku_net_error("tls_limit_exceeded", "native TLS safety limit was exceeded");
  if (status == KU_TLS_STATUS_PANIC)
    return ku_net_error("tls_error", "native TLS runtime panicked");
  return ku_net_error("tls_error", "native TLS operation failed");
}

static void ku_net_tls_session_drop(KuNetClient* client) {
  if (!client) return;
  if (client->tls_session) {
    KuTlsClientSession* session = client->tls_session;
    client->tls_session = NULL;
    (void)ku_tls_v1_client_drop(session);
  }
  if (client->tls_pending) {
    free(client->tls_pending);
    client->tls_pending = NULL;
  }
  client->tls_pending_len = 0;
  client->tls_pending_offset = 0;
}

static int ku_net_tls_send_ciphertext(KuNetClient* client,
    const uint8_t* ciphertext, size_t length,
    unsigned long long deadline, const char* timeout_code,
    const char* timeout_message, KuError* error) {
  size_t offset = 0;
  while (offset < length) {
    int waited = ku_net_socket_wait(client->socket_value, 1, deadline);
    if (waited != 1) {
      *error = waited == 0 ? ku_net_error(timeout_code, timeout_message)
          : ku_net_error("transport_error", "net TLS write readiness failed");
      return 0;
    }
    int amount = ku_net_socket_send(client->socket_value,
        ciphertext + offset, length - offset);
    if (amount > 0) { offset += (size_t)amount; continue; }
    if (amount < 0) {
      int socket_error = ku_net_socket_last_error();
      if (ku_net_socket_error_interrupted(socket_error)
          || ku_net_socket_error_would_block(socket_error)) continue;
    }
    *error = ku_net_error("transport_error", "net TLS ciphertext write failed");
    return 0;
  }
  return 1;
}

static int ku_net_tls_drain(KuNetClient* client, unsigned long long deadline,
    const char* timeout_code, const char* timeout_message, KuError* error) {
  uint8_t ciphertext[KU_TLS_MAX_IO_BYTES];
  for (uint32_t step = 0; step < KU_NET_TLS_MAX_DRAIN_STEPS; step++) {
    if (ku_net_now_ms() >= deadline) {
      *error = ku_net_error(timeout_code, timeout_message); return 0;
    }
    uint32_t wants_write = 0;
    uint32_t status = ku_tls_v1_client_wants_write(
        client->tls_session, &wants_write);
    if (status != KU_TLS_STATUS_OK) {
      *error = ku_net_tls_status_error(status, "wants_write"); return 0;
    }
    if (!wants_write) {
      if (ku_net_now_ms() >= deadline) {
        *error = ku_net_error(timeout_code, timeout_message); return 0;
      }
      return 1;
    }
    size_t written = 0;
    status = ku_tls_v1_client_drain_ciphertext(client->tls_session,
        ciphertext, sizeof(ciphertext), &written);
    if (status != KU_TLS_STATUS_OK || written == 0
        || written > KU_TLS_MAX_IO_BYTES) {
      *error = status == KU_TLS_STATUS_OK
          ? ku_net_error("tls_error", "native TLS drain made no valid progress")
          : ku_net_tls_status_error(status, "drain");
      return 0;
    }
    if (!ku_net_tls_send_ciphertext(client, ciphertext, written, deadline,
            timeout_code, timeout_message, error)) return 0;
  }
  if (ku_net_now_ms() >= deadline) {
    *error = ku_net_error(timeout_code, timeout_message); return 0;
  }
  uint32_t wants_write = 0;
  uint32_t status = ku_tls_v1_client_wants_write(
      client->tls_session, &wants_write);
  if (status != KU_TLS_STATUS_OK) {
    *error = ku_net_tls_status_error(status, "wants_write"); return 0;
  }
  if (!wants_write) return 1;
  *error = ku_net_error("tls_limit_exceeded", "native TLS drain exceeded its progress bound");
  return 0;
}

/* Returns 1 after ciphertext/process progress, 2 after authenticated peer
   close, 3 for a bounded retry without progress, and 0 on error. Every
   successful feed is followed by process exactly once before another feed.
   out_wire_bytes counts only bytes newly read from the socket; pending bytes
   were already charged when their original recv completed. */
static int ku_net_tls_receive_process(KuNetClient* client,
    unsigned long long deadline, const char* timeout_code,
    const char* timeout_message, size_t max_new_wire_bytes,
    size_t* out_wire_bytes, KuError* error) {
  *out_wire_bytes = 0;
  uint32_t wants_read = 0;
  uint32_t status = ku_tls_v1_client_wants_read(client->tls_session, &wants_read);
  if (status != KU_TLS_STATUS_OK) {
    *error = ku_net_tls_status_error(status, "wants_read"); return 0;
  }
  if (!wants_read) {
    *error = ku_net_error("tls_error", "native TLS requested no bounded I/O progress");
    return 0;
  }
  if (client->tls_pending) {
    size_t remaining = client->tls_pending_len - client->tls_pending_offset;
    size_t consumed = 0;
    status = ku_tls_v1_client_feed_ciphertext(client->tls_session,
        client->tls_pending + client->tls_pending_offset, remaining, &consumed);
    if (status != KU_TLS_STATUS_OK || consumed == 0 || consumed > remaining) {
      *error = status == KU_TLS_STATUS_OK
          ? ku_net_error("tls_error", "native TLS pending feed made no valid progress")
          : ku_net_tls_status_error(status, "feed");
      return 0;
    }
    client->tls_pending_offset += consumed;
    status = ku_tls_v1_client_process(client->tls_session);
    if (status != KU_TLS_STATUS_OK) {
      *error = ku_net_tls_status_error(status, "process"); return 0;
    }
    if (client->tls_pending_offset == client->tls_pending_len) {
      free(client->tls_pending);
      client->tls_pending = NULL;
      client->tls_pending_len = 0;
      client->tls_pending_offset = 0;
    }
    return 1;
  }
  if (max_new_wire_bytes == 0) {
    *error = ku_net_error("tls_limit_exceeded",
        "native TLS ciphertext budget is exhausted"); return 0;
  }
  int waited = ku_net_socket_wait(client->socket_value, 0, deadline);
  if (waited != 1) {
    *error = waited == 0 ? ku_net_error(timeout_code, timeout_message)
        : ku_net_error("transport_error", "net TLS read readiness failed");
    return 0;
  }
  uint8_t ciphertext[KU_TLS_MAX_IO_BYTES];
  size_t receive_capacity = max_new_wire_bytes < sizeof(ciphertext)
      ? max_new_wire_bytes : sizeof(ciphertext);
  int received = ku_net_socket_recv(client->socket_value,
      ciphertext, receive_capacity);
  if (received == 0) {
    status = ku_tls_v1_client_notify_eof(client->tls_session);
    if (status == KU_TLS_STATUS_OK) return 2;
    *error = ku_net_tls_status_error(status, "notify_eof");
    return 0;
  }
  if (received < 0) {
    int socket_error = ku_net_socket_last_error();
    if (ku_net_socket_error_interrupted(socket_error)
        || ku_net_socket_error_would_block(socket_error)) return 3;
    *error = ku_net_error("transport_error", "net TLS ciphertext read failed");
    return 0;
  }
  *out_wire_bytes = (size_t)received;
  size_t offset = 0;
  uint32_t feed_steps = 0;
  while (offset < (size_t)received) {
    if (feed_steps >= KU_TLS_MAX_IO_BYTES) {
      size_t pending_len = (size_t)received - offset;
      client->tls_pending = (uint8_t*)malloc(pending_len);
      if (!client->tls_pending) {
        *error = ku_net_error("out_of_memory", "net TLS pending input allocation failed");
        return 0;
      }
      memcpy(client->tls_pending, ciphertext + offset, pending_len);
      client->tls_pending_len = pending_len;
      client->tls_pending_offset = 0;
      return 1;
    }
    feed_steps++;
    status = ku_tls_v1_client_wants_read(client->tls_session, &wants_read);
    if (status != KU_TLS_STATUS_OK || !wants_read) {
      *error = status == KU_TLS_STATUS_OK
          ? ku_net_error("tls_error", "native TLS left ciphertext unconsumed")
          : ku_net_tls_status_error(status, "wants_read");
      return 0;
    }
    size_t consumed = 0;
    status = ku_tls_v1_client_feed_ciphertext(client->tls_session,
        ciphertext + offset, (size_t)received - offset, &consumed);
    if (status != KU_TLS_STATUS_OK || consumed == 0
        || consumed > (size_t)received - offset) {
      *error = status == KU_TLS_STATUS_OK
          ? ku_net_error("tls_error", "native TLS feed made no valid progress")
          : ku_net_tls_status_error(status, "feed");
      return 0;
    }
    offset += consumed;
    status = ku_tls_v1_client_process(client->tls_session);
    if (status != KU_TLS_STATUS_OK) {
      *error = ku_net_tls_status_error(status, "process"); return 0;
    }
    if (offset < (size_t)received) {
      status = ku_tls_v1_client_wants_read(client->tls_session, &wants_read);
      if (status != KU_TLS_STATUS_OK) {
        *error = ku_net_tls_status_error(status, "wants_read"); return 0;
      }
      if (!wants_read) {
        size_t pending_len = (size_t)received - offset;
        client->tls_pending = (uint8_t*)malloc(pending_len);
        if (!client->tls_pending) {
          *error = ku_net_error("out_of_memory", "net TLS pending input allocation failed");
          return 0;
        }
        memcpy(client->tls_pending, ciphertext + offset, pending_len);
        client->tls_pending_len = pending_len;
        client->tls_pending_offset = 0;
        return 1;
      }
    }
  }
  return 1;
}

static int ku_net_tls_handshake_until(KuNetClient* client,
    unsigned long long deadline, KuError* error) {
  unsigned long long driver_calls = 0;
  unsigned long long wire_bytes = 0;
  uint32_t no_progress_calls = 0;
  for (;;) {
    if (ku_net_now_ms() >= deadline) {
      *error = ku_net_error("connect_timeout", "net TLS handshake timed out"); return 0;
    }
    if (!ku_net_tls_drain(client, deadline, "connect_timeout",
            "net TLS handshake timed out", error)) return 0;
    uint32_t handshaking = 0;
    uint32_t status = ku_tls_v1_client_is_handshaking(
        client->tls_session, &handshaking);
    if (status != KU_TLS_STATUS_OK) {
      *error = ku_net_tls_status_error(status, "is_handshaking"); return 0;
    }
    if (!handshaking) {
      if (ku_net_now_ms() >= deadline) {
        *error = ku_net_error("connect_timeout", "net TLS handshake timed out"); return 0;
      }
      return 1;
    }
    if (driver_calls >= KU_NET_TLS_MAX_HANDSHAKE_DRIVER_CALLS) {
      *error = ku_net_error("tls_limit_exceeded",
          "native TLS handshake exceeded its driver-call bound"); return 0;
    }
    if (!client->tls_pending
        && wire_bytes >= KU_NET_TLS_MAX_HANDSHAKE_CIPHERTEXT_BYTES) {
      *error = ku_net_error("tls_limit_exceeded",
          "native TLS handshake exceeded its ciphertext bound"); return 0;
    }
    size_t newly_received = 0;
    size_t receive_budget = client->tls_pending ? 0
        : (size_t)(KU_NET_TLS_MAX_HANDSHAKE_CIPHERTEXT_BYTES - wire_bytes);
    int progressed = ku_net_tls_receive_process(client, deadline,
        "connect_timeout", "net TLS handshake timed out", receive_budget,
        &newly_received, error);
    driver_calls++;
    if (progressed == 0) return 0;
    if (progressed == 2) {
      *error = ku_net_error("tls_truncated", "net TLS peer closed during handshake");
      return 0;
    }
    if (progressed == 3) {
      no_progress_calls++;
      if (no_progress_calls > KU_NET_TLS_MAX_NO_PROGRESS_CALLS) {
        *error = ku_net_error("tls_limit_exceeded",
            "native TLS handshake made no bounded input progress"); return 0;
      }
      continue;
    }
    if ((unsigned long long)newly_received
        > KU_NET_TLS_MAX_HANDSHAKE_CIPHERTEXT_BYTES - wire_bytes) {
      *error = ku_net_error("tls_limit_exceeded",
          "native TLS handshake exceeded its ciphertext bound"); return 0;
    }
    wire_bytes += (unsigned long long)newly_received;
  }
}

static int ku_net_tls_write_until(KuNetClient* client, const uint8_t* data,
    size_t len, unsigned long long deadline, KuError* error) {
  size_t offset = 0;
  unsigned long long driver_calls = 0;
  unsigned long long wire_bytes = 0;
  uint32_t no_progress_calls = 0;
  while (offset < len) {
    if (ku_net_now_ms() >= deadline) {
      *error = ku_net_error("write_timeout", "net write timed out"); return 0;
    }
    size_t chunk = len - offset > KU_TLS_MAX_IO_BYTES
        ? KU_TLS_MAX_IO_BYTES : len - offset;
    size_t written = 0;
    uint32_t status = ku_tls_v1_client_write_plaintext(client->tls_session,
        data + offset, chunk, &written);
    if (status == KU_TLS_STATUS_OK) {
      if (written == 0 || written > chunk) {
        *error = ku_net_error("tls_error", "native TLS plaintext write made no valid progress");
        return 0;
      }
      offset += written;
    } else if (status != KU_TLS_STATUS_WOULD_BLOCK) {
      *error = ku_net_tls_status_error(status, "write_plaintext"); return 0;
    }
    if (!ku_net_tls_drain(client, deadline, "write_timeout",
            "net write timed out", error)) return 0;
    if (status == KU_TLS_STATUS_WOULD_BLOCK) {
      if (driver_calls >= KU_NET_TLS_MAX_OPERATION_DRIVER_CALLS) {
        *error = ku_net_error("tls_limit_exceeded",
            "native TLS write exceeded its driver-call bound"); return 0;
      }
      if (!client->tls_pending
          && wire_bytes >= KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES) {
        *error = ku_net_error("tls_limit_exceeded",
            "native TLS write exceeded its ciphertext bound"); return 0;
      }
      size_t newly_received = 0;
      size_t receive_budget = client->tls_pending ? 0
          : (size_t)(KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES - wire_bytes);
      int progressed = ku_net_tls_receive_process(client, deadline,
          "write_timeout", "net write timed out", receive_budget,
          &newly_received, error);
      driver_calls++;
      if (progressed == 0) return 0;
      if (progressed == 2) {
        *error = ku_net_error("end_of_stream", "net TLS peer closed the stream");
        return 0;
      }
      if (progressed == 3) {
        no_progress_calls++;
        if (no_progress_calls > KU_NET_TLS_MAX_NO_PROGRESS_CALLS) {
          *error = ku_net_error("tls_limit_exceeded",
              "native TLS write made no bounded input progress"); return 0;
        }
        continue;
      }
      if ((unsigned long long)newly_received
          > KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES - wire_bytes) {
        *error = ku_net_error("tls_limit_exceeded",
            "native TLS write exceeded its ciphertext bound"); return 0;
      }
      wire_bytes += (unsigned long long)newly_received;
    }
  }
  if (ku_net_now_ms() >= deadline) {
    *error = ku_net_error("write_timeout", "net write timed out"); return 0;
  }
  return 1;
}

static int ku_net_tls_read_until(KuNetClient* client, uint8_t* data,
    size_t capacity, unsigned long long deadline, size_t* out_read,
    KuError* error) {
  *out_read = 0;
  size_t bounded_capacity = capacity > KU_TLS_MAX_IO_BYTES
      ? KU_TLS_MAX_IO_BYTES : capacity;
  unsigned long long driver_calls = 0;
  unsigned long long wire_bytes = 0;
  uint32_t no_progress_calls = 0;
  for (;;) {
    if (ku_net_now_ms() >= deadline) {
      *error = ku_net_error("read_timeout", "net read timed out"); return 0;
    }
    size_t amount = 0;
    uint32_t status = ku_tls_v1_client_read_plaintext(client->tls_session,
        data, bounded_capacity, &amount);
    if (status == KU_TLS_STATUS_OK) {
      if (amount > bounded_capacity) {
        *error = ku_net_error("tls_error", "native TLS reported an invalid plaintext length");
        return 0;
      }
      if (amount > 0) {
        if (ku_net_now_ms() >= deadline) {
          *error = ku_net_error("read_timeout", "net read timed out"); return 0;
        }
        *out_read = amount; return 1;
      }
      uint32_t peer_closed = 0;
      status = ku_tls_v1_client_peer_closed(client->tls_session, &peer_closed);
      if (status != KU_TLS_STATUS_OK) {
        *error = ku_net_tls_status_error(status, "peer_closed"); return 0;
      }
      if (peer_closed) {
        *error = ku_net_error("end_of_stream", "net TLS peer closed the stream");
        return 0;
      }
    } else if (status != KU_TLS_STATUS_WOULD_BLOCK) {
      *error = ku_net_tls_status_error(status, "read_plaintext"); return 0;
    }
    if (!ku_net_tls_drain(client, deadline, "read_timeout",
            "net read timed out", error)) return 0;
    if (driver_calls >= KU_NET_TLS_MAX_OPERATION_DRIVER_CALLS) {
      *error = ku_net_error("tls_limit_exceeded",
          "native TLS read exceeded its driver-call bound"); return 0;
    }
    if (!client->tls_pending
        && wire_bytes >= KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES) {
      *error = ku_net_error("tls_limit_exceeded",
          "native TLS read exceeded its ciphertext bound"); return 0;
    }
    size_t newly_received = 0;
    size_t receive_budget = client->tls_pending ? 0
        : (size_t)(KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES - wire_bytes);
    int progressed = ku_net_tls_receive_process(client, deadline,
        "read_timeout", "net read timed out", receive_budget,
        &newly_received, error);
    driver_calls++;
    if (progressed == 0) return 0;
    if (progressed == 2) continue;
    if (progressed == 3) {
      no_progress_calls++;
      if (no_progress_calls > KU_NET_TLS_MAX_NO_PROGRESS_CALLS) {
        *error = ku_net_error("tls_limit_exceeded",
            "native TLS read made no bounded input progress"); return 0;
      }
      continue;
    }
    if ((unsigned long long)newly_received
        > KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES - wire_bytes) {
      *error = ku_net_error("tls_limit_exceeded",
          "native TLS read exceeded its ciphertext bound"); return 0;
    }
    wire_bytes += (unsigned long long)newly_received;
  }
}
#endif

static int ku_net_gate_init(KuNetGate* gate) {
#if defined(_WIN32)
  gate->semaphore = CreateSemaphoreW(NULL, 1, 1, NULL);
  return gate->semaphore ? 0 : -1;
#else
  gate->available = 1;
  if (pthread_mutex_init(&gate->mutex, NULL) != 0) return -1;
  int result;
#if defined(__APPLE__)
  result = pthread_cond_init(&gate->condition, NULL);
#else
  pthread_condattr_t attributes;
  result = pthread_condattr_init(&attributes);
  if (result == 0) {
    result = pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
    if (result == 0) result = pthread_cond_init(&gate->condition, &attributes);
    pthread_condattr_destroy(&attributes);
  }
#endif
  if (result != 0) pthread_mutex_destroy(&gate->mutex);
  return result == 0 ? 0 : -1;
#endif
}

/* 1=owned, 0=deadline, -1=synchronization error. */
static int ku_net_gate_acquire(KuNetGate* gate, unsigned long long deadline) {
  if (!gate || deadline == 0) return -1;
#if defined(_WIN32)
  unsigned long long now = ku_net_now_ms();
  if (now >= deadline) return 0;
  unsigned long long remaining = deadline - now;
  DWORD wait_ms = remaining > (unsigned long long)UINT32_MAX
      ? UINT32_MAX : (DWORD)remaining;
  DWORD result = WaitForSingleObject(gate->semaphore, wait_ms);
  if (result != WAIT_OBJECT_0) return result == WAIT_TIMEOUT ? 0 : -1;
  if (ku_net_now_ms() >= deadline) {
    return ReleaseSemaphore(gate->semaphore, 1, NULL) ? 0 : -1;
  }
  return 1;
#else
  if (pthread_mutex_lock(&gate->mutex) != 0) return -1;
  while (!gate->available) {
    unsigned long long now = ku_net_now_ms();
    if (now >= deadline)
      return pthread_mutex_unlock(&gate->mutex) == 0 ? 0 : -1;
    unsigned long long remaining = deadline - now;
    int result;
#if defined(__APPLE__)
    struct timespec relative = {
      (time_t)(remaining / 1000ULL),
      (long)((remaining % 1000ULL) * 1000000ULL)
    };
    result = pthread_cond_timedwait_relative_np(
        &gate->condition, &gate->mutex, &relative);
#else
    struct timespec absolute = {0};
    if (clock_gettime(CLOCK_MONOTONIC, &absolute) != 0) {
      pthread_mutex_unlock(&gate->mutex); return -1;
    }
    absolute.tv_sec += (time_t)(remaining / 1000ULL);
    long extra = (long)((remaining % 1000ULL) * 1000000ULL);
    if (absolute.tv_nsec > 999999999L - extra) {
      absolute.tv_sec++; absolute.tv_nsec -= 1000000000L - extra;
    } else {
      absolute.tv_nsec += extra;
    }
    result = pthread_cond_timedwait(&gate->condition, &gate->mutex, &absolute);
#endif
    if (result == ETIMEDOUT)
      return pthread_mutex_unlock(&gate->mutex) == 0 ? 0 : -1;
    if (result != 0) { pthread_mutex_unlock(&gate->mutex); return -1; }
  }
  if (ku_net_now_ms() >= deadline) {
    return pthread_mutex_unlock(&gate->mutex) == 0 ? 0 : -1;
  }
  gate->available = 0;
  if (pthread_mutex_unlock(&gate->mutex) != 0) return -1;
  return 1;
#endif
}

static int ku_net_gate_release(KuNetGate* gate) {
#if defined(_WIN32)
  return gate && ReleaseSemaphore(gate->semaphore, 1, NULL) ? 0 : -1;
#else
  if (!gate || pthread_mutex_lock(&gate->mutex) != 0) return -1;
  if (gate->available) { pthread_mutex_unlock(&gate->mutex); return -1; }
  gate->available = 1;
  int signal_result = pthread_cond_signal(&gate->condition);
  int unlock_result = pthread_mutex_unlock(&gate->mutex);
  return signal_result == 0 && unlock_result == 0 ? 0 : -1;
#endif
}

static void ku_net_gate_destroy(KuNetGate* gate) {
  if (!gate) return;
#if defined(_WIN32)
  if (gate->semaphore) CloseHandle(gate->semaphore);
  gate->semaphore = NULL;
#else
  pthread_cond_destroy(&gate->condition);
  pthread_mutex_destroy(&gate->mutex);
#endif
}

static void ku_net_poison(KuNetClient* client) {
  if (!client) return;
  ku_net_atomic_flag_set(&client->poison_requested);
#if defined(KU_NATIVE_TLS_ENABLED)
  ku_net_tls_session_drop(client);
#endif
  if (client && client->socket_value != KU_NET_INVALID_SOCKET) {
    ku_net_socket_close(client->socket_value);
    client->socket_value = KU_NET_INVALID_SOCKET;
  }
}

static KuValue* ku_net_config_get(KuObject* config, const char* key) {
  return config ? ku_object_get(config,
      ku_string_static((const uint8_t*)key, strlen(key))) : NULL;
}

static int ku_net_config_key_known(KuString key) {
  static const char* fields[] = {
    "host", "port", "connect_timeout_ms", "read_timeout_ms",
    "write_timeout_ms", "max_read_bytes", "tls", "tls_server_name",
    "tls_ca_pem"
  };
  for (size_t index = 0; index < sizeof(fields) / sizeof(fields[0]); index++) {
    size_t len = strlen(fields[index]);
    if (key.len == len && key.ptr && memcmp(key.ptr, fields[index], len) == 0) return 1;
  }
  return 0;
}

static int ku_net_config_bool(KuObject* config, const char* key,
    int default_value, int* out, KuError* error) {
  KuValue* value = ku_net_config_get(config, key);
  if (!value) { *out = default_value; return 1; }
  if (value->tag != KU_BOOL) {
    *error = ku_net_error("invalid_config", "net client boolean config must be bool");
    return 0;
  }
  *out = value->as.b ? 1 : 0;
  return 1;
}

static int ku_net_config_tls_string(KuObject* config, const char* key,
    size_t max_len, KuString* out, KuError* error) {
  KuValue* value = ku_net_config_get(config, key);
  *out = (KuString){0};
  if (!value) return 1;
  if (value->tag != KU_STR || !value->as.s.ptr || value->as.s.len == 0
      || value->as.s.len > max_len
      || memchr(value->as.s.ptr, 0, value->as.s.len) != NULL) {
    *error = ku_net_error("invalid_config", "net client TLS string config is invalid or too large");
    return 0;
  }
  *out = value->as.s;
  return 1;
}

static int ku_net_config_int(KuObject* config, const char* key,
    uint32_t default_value, uint32_t min, uint32_t max,
    uint32_t* out, KuError* error) {
  KuValue* value = ku_net_config_get(config, key);
  if (!value) { *out = default_value; return 1; }
  if (value->tag != KU_INT || value->as.i < (int64_t)min || value->as.i > (int64_t)max) {
    *error = ku_net_error("invalid_config", "net client integer config is outside its supported range");
    return 0;
  }
  *out = (uint32_t)value->as.i;
  return 1;
}

static int ku_net_host_valid(KuString host) {
  if (!host.ptr || host.len == 0 || host.len > KU_NET_MAX_HOST_BYTES) return 0;
  for (size_t index = 0; index < host.len; index++) {
    /* Native transports accept one portable ASCII spelling. International
       domain names must be supplied as ASCII-compatible (punycode) labels. */
    if (host.ptr[index] < 0x21 || host.ptr[index] > 0x7e) return 0;
  }
  return 1;
}

static KuResult_net_client ku_net_client(KuObject* config) {
  if (!config) return (KuResult_net_client){ false, NULL,
    ku_net_error("invalid_config", "net.client requires a config object") };
  for (size_t index = 0; index < config->cap; index++) {
    if (config->entries[index].used
        && !ku_net_config_key_known(config->entries[index].key)) {
      return (KuResult_net_client){ false, NULL,
        ku_net_error("invalid_config", "net.client config contains an unknown field") };
    }
  }
  KuValue* host_value = ku_net_config_get(config, "host");
  KuValue* port_value = ku_net_config_get(config, "port");
  if (!host_value || host_value->tag != KU_STR
      || !ku_net_host_valid(host_value->as.s)) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("invalid_config", "net.client requires a valid non-empty string host") };
  }
  if (!port_value || port_value->tag != KU_INT
      || port_value->as.i < 1 || port_value->as.i > 65535) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("invalid_config", "net.client requires port between 1 and 65535") };
  }
  KuError config_error = (KuError){0};
  uint32_t connect_timeout_ms, read_timeout_ms, write_timeout_ms, max_read_bytes;
  int tls_enabled = 0;
  KuString tls_server_name = (KuString){0};
  KuString tls_ca_pem = (KuString){0};
  if (!ku_net_config_int(config, "connect_timeout_ms", KU_NET_DEFAULT_TIMEOUT_MS,
          1, KU_NET_MAX_TIMEOUT_MS, &connect_timeout_ms, &config_error)
      || !ku_net_config_int(config, "read_timeout_ms", KU_NET_DEFAULT_TIMEOUT_MS,
          1, KU_NET_MAX_TIMEOUT_MS, &read_timeout_ms, &config_error)
      || !ku_net_config_int(config, "write_timeout_ms", KU_NET_DEFAULT_TIMEOUT_MS,
          1, KU_NET_MAX_TIMEOUT_MS, &write_timeout_ms, &config_error)
      || !ku_net_config_int(config, "max_read_bytes", KU_NET_DEFAULT_MAX_READ_BYTES,
          1, KU_NET_MAX_READ_BYTES, &max_read_bytes, &config_error)
      || !ku_net_config_bool(config, "tls", 0, &tls_enabled, &config_error)
      || !ku_net_config_tls_string(config, "tls_server_name", KU_NET_MAX_HOST_BYTES,
          &tls_server_name, &config_error)
      || !ku_net_config_tls_string(config, "tls_ca_pem", KU_NET_MAX_TLS_CA_PEM_BYTES,
          &tls_ca_pem, &config_error)) {
    return (KuResult_net_client){ false, NULL, config_error };
  }
  if (!tls_enabled && (tls_server_name.len != 0 || tls_ca_pem.len != 0)) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("invalid_config", "net TLS fields require tls to be true") };
  }
  if (tls_enabled && tls_server_name.len == 0) tls_server_name = host_value->as.s;
  if (tls_enabled && !ku_net_host_valid(tls_server_name)) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("invalid_config", "net TLS server name is invalid") };
  }
#if !defined(KU_NATIVE_TLS_ENABLED)
  if (tls_enabled) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("tls_unavailable", "native TLS is unavailable for this target") };
  }
#else
  if (tls_enabled && !ku_net_tls_check_abi()) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("tls_unavailable", "native TLS ABI or build identifier mismatch") };
  }
#endif

  unsigned long long deadline = ku_net_deadline_after_ms(connect_timeout_ms);
  if (deadline == 0 || ku_net_now_ms() >= deadline) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("connect_timeout", "net connect timed out") };
  }
  if (ku_net_socket_startup() != 0) {
    return (KuResult_net_client){ false, NULL,
      ku_net_error("transport_error", "network runtime initialization failed") };
  }
  size_t hostname_bytes = host_value->as.s.len + 1;
  char* hostname = (char*)malloc(hostname_bytes);
  if (!hostname) return (KuResult_net_client){ false, NULL,
    ku_net_error("out_of_memory", "net host allocation failed") };
  memcpy(hostname, host_value->as.s.ptr, host_value->as.s.len);
  hostname[host_value->as.s.len] = '\0';
  char port_text[6];
  int port_length = snprintf(port_text, sizeof(port_text), "%lld",
      (long long)port_value->as.i);
  if (port_length <= 0 || (size_t)port_length >= sizeof(port_text)) {
    free(hostname);
    return (KuResult_net_client){ false, NULL,
      ku_net_error("invalid_config", "net.client port is invalid") };
  }
  struct addrinfo hints;
  memset(&hints, 0, sizeof(hints));
  hints.ai_family = AF_UNSPEC;
  hints.ai_socktype = SOCK_STREAM;
  hints.ai_protocol = IPPROTO_TCP;
  struct addrinfo* addresses = NULL;
  int resolver_status = getaddrinfo(hostname, port_text, &hints, &addresses);
  free(hostname);
  if (ku_net_now_ms() >= deadline) {
    if (addresses) freeaddrinfo(addresses);
    return (KuResult_net_client){ false, NULL,
      ku_net_error("connect_timeout", "net connect timed out") };
  }
  if (resolver_status != 0 || !addresses) {
    if (addresses) freeaddrinfo(addresses);
    return (KuResult_net_client){ false, NULL,
      resolver_status == EAI_MEMORY
        ? ku_net_error("out_of_memory", "net host resolution allocation failed")
        : ku_net_error("connect_error", "net host resolution failed") };
  }

  KuNetSocket connected = KU_NET_INVALID_SOCKET;
  int deadline_expired = 0;
  uint32_t attempted_addresses = 0;
  for (struct addrinfo* address = addresses; address; address = address->ai_next) {
    if (attempted_addresses >= KU_NET_MAX_RESOLVED_ADDRESSES) break;
    attempted_addresses++;
    if (ku_net_now_ms() >= deadline) { deadline_expired = 1; break; }
    KuNetSocket candidate = socket(address->ai_family, address->ai_socktype,
        address->ai_protocol);
    if (candidate == KU_NET_INVALID_SOCKET) continue;
    if (ku_net_socket_suppress_sigpipe(candidate) != 0
        || ku_net_socket_set_nonblocking(candidate) != 0) {
      ku_net_socket_close(candidate); continue;
    }
    int connect_status = ku_net_socket_connect(candidate, address->ai_addr,
        (size_t)address->ai_addrlen);
    int ready = connect_status == 0;
    if (!ready) {
      int connect_error = ku_net_socket_last_error();
      if (ku_net_socket_error_connecting(connect_error)) {
        int waited = ku_net_socket_wait(candidate, 1, deadline);
        if (waited == 0) deadline_expired = 1;
        if (waited == 1) {
          int pending_error = 0;
          ready = ku_net_socket_pending_error(candidate, &pending_error) == 0
              && pending_error == 0;
        }
      }
    }
    if (ready && ku_net_now_ms() < deadline) { connected = candidate; break; }
    ku_net_socket_close(candidate);
    if (deadline_expired || ku_net_now_ms() >= deadline) {
      deadline_expired = 1; break;
    }
  }
  freeaddrinfo(addresses);
  if (connected == KU_NET_INVALID_SOCKET) {
    return (KuResult_net_client){ false, NULL,
      deadline_expired
        ? ku_net_error("connect_timeout", "net connect timed out")
        : ku_net_error("connect_error", "net connection failed") };
  }
  if (ku_net_now_ms() >= deadline) {
    ku_net_socket_close(connected);
    return (KuResult_net_client){ false, NULL,
      ku_net_error("connect_timeout", "net connect timed out") };
  }
  KuNetClient* client = (KuNetClient*)malloc(sizeof(KuNetClient));
  if (!client) {
    ku_net_socket_close(connected);
    return (KuResult_net_client){ false, NULL,
      ku_net_error("out_of_memory", "net client allocation failed") };
  }
  memset(client, 0, sizeof(*client));
  ku_net_atomic_flag_init(&client->poison_requested);
  client->socket_value = connected;
  client->tls_enabled = tls_enabled ? 1 : 0;
  client->read_timeout_ms = read_timeout_ms;
  client->write_timeout_ms = write_timeout_ms;
  client->max_read_bytes = max_read_bytes;
#if defined(KU_NATIVE_TLS_ENABLED)
  if (tls_enabled) {
    KuTlsConfig* tls_config = NULL;
    uint32_t root_mode = tls_ca_pem.len == 0
        ? KU_TLS_ROOTS_WEBPKI : KU_TLS_ROOTS_CUSTOM_PEM;
    uint32_t status = ku_tls_v1_config_new(root_mode,
        tls_ca_pem.len == 0 ? NULL : tls_ca_pem.ptr, tls_ca_pem.len,
        &tls_config);
    if (status != KU_TLS_STATUS_OK || !tls_config) {
      if (tls_config) (void)ku_tls_v1_config_drop(tls_config);
      ku_net_poison(client); free(client);
      return (KuResult_net_client){ false, NULL,
        ku_net_tls_status_error(status, "config_new") };
    }
    status = ku_tls_v1_client_new(tls_config, tls_server_name.ptr,
        tls_server_name.len, &client->tls_session);
    uint32_t config_drop_status = ku_tls_v1_config_drop(tls_config);
    tls_config = NULL;
    if (status != KU_TLS_STATUS_OK || config_drop_status != KU_TLS_STATUS_OK
        || !client->tls_session) {
      KuError tls_error = status != KU_TLS_STATUS_OK
          ? ku_net_tls_status_error(status, "client_new")
          : config_drop_status != KU_TLS_STATUS_OK
              ? ku_net_tls_status_error(config_drop_status, "config_drop")
              : ku_net_error("tls_error", "native TLS returned no session");
      ku_net_poison(client); free(client);
      return (KuResult_net_client){ false, NULL, tls_error };
    }
    KuError tls_error = (KuError){0};
    if (!ku_net_tls_handshake_until(client, deadline, &tls_error)) {
      ku_net_poison(client); free(client);
      return (KuResult_net_client){ false, NULL, tls_error };
    }
  }
#endif
  if (ku_net_gate_init(&client->gate) != 0) {
    ku_net_poison(client); free(client);
    return (KuResult_net_client){ false, NULL,
      ku_net_error("sync_error", "net client synchronization initialization failed") };
  }
  if (ku_net_now_ms() >= deadline) {
    ku_net_poison(client);
    ku_net_gate_destroy(&client->gate);
    free(client);
    return (KuResult_net_client){ false, NULL,
      ku_net_error("connect_timeout", "net connect timed out") };
  }
  return (KuResult_net_client){ true, client, (KuError){0} };
}

static KuResult_null ku_net_write(KuNetClient* client, KuBytes data) {
  if (!client || ku_net_atomic_flag_load(&client->poison_requested)) {
    return (KuResult_null){ false, 0,
      ku_net_error("client_closed", "net client is closed") };
  }
  if ((data.len != 0 && !data.ptr) || data.len > (size_t)KU_BYTES_MAX_LENGTH) {
    return (KuResult_null){ false, 0,
      ku_net_error("invalid_bytes", "net write received invalid or oversized bytes") };
  }
  unsigned long long deadline = ku_net_deadline_after_ms(client->write_timeout_ms);
  int acquired = ku_net_gate_acquire(&client->gate, deadline);
  if (acquired != 1) {
    if (acquired < 0) ku_net_atomic_flag_set(&client->poison_requested);
    return (KuResult_null){ false, 0, acquired == 0
      ? ku_net_error("write_timeout", "net write timed out")
      : ku_net_error("sync_error", "net client synchronization failed") };
  }
  if (ku_net_atomic_flag_load(&client->poison_requested)
      || client->socket_value == KU_NET_INVALID_SOCKET) {
    ku_net_poison(client);
    if (ku_net_gate_release(&client->gate) != 0) {
      return (KuResult_null){ false, 0,
        ku_net_error("sync_error", "net client synchronization failed") };
    }
    return (KuResult_null){ false, 0,
      ku_net_error("client_closed", "net client is closed") };
  }
  KuError error = (KuError){0};
#if defined(KU_NATIVE_TLS_ENABLED)
  if (client->tls_enabled) {
    if (!client->tls_session
        || !ku_net_tls_write_until(client, data.ptr, data.len, deadline, &error))
      ku_net_poison(client);
  } else {
#endif
  size_t sent = 0;
  while (sent < data.len) {
    int waited = ku_net_socket_wait(client->socket_value, 1, deadline);
    if (waited != 1) {
      error = waited == 0
        ? ku_net_error("write_timeout", "net write timed out")
        : ku_net_error("transport_error", "net write readiness failed");
      ku_net_poison(client); break;
    }
    int amount = ku_net_socket_send(client->socket_value, data.ptr + sent,
        data.len - sent);
    if (amount > 0) { sent += (size_t)amount; continue; }
    if (amount < 0) {
      int socket_error = ku_net_socket_last_error();
      if (ku_net_socket_error_interrupted(socket_error)
          || ku_net_socket_error_would_block(socket_error)) continue;
    }
    error = ku_net_error("transport_error", "net write failed");
    ku_net_poison(client); break;
  }
#if defined(KU_NATIVE_TLS_ENABLED)
  }
#endif
  if (error.code.len == 0 && ku_net_now_ms() >= deadline) {
    error = ku_net_error("write_timeout", "net write timed out");
    ku_net_poison(client);
  }
  if (ku_net_gate_release(&client->gate) != 0) {
    ku_net_atomic_flag_set(&client->poison_requested);
    if (error.code.len == 0)
      error = ku_net_error("sync_error", "net client synchronization failed");
  }
  if (error.code.len != 0) return (KuResult_null){ false, 0, error };
  return (KuResult_null){ true, 0, (KuError){0} };
}

static KuResult_bytes ku_net_read(KuNetClient* client, int64_t requested) {
  if (!client || ku_net_atomic_flag_load(&client->poison_requested)) {
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_net_error("client_closed", "net client is closed") };
  }
  if (requested < 1 || (uint64_t)requested > (uint64_t)client->max_read_bytes) {
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_net_error("invalid_read_size", "net read size is outside the configured bound") };
  }
  unsigned long long deadline = ku_net_deadline_after_ms(client->read_timeout_ms);
  int acquired = ku_net_gate_acquire(&client->gate, deadline);
  if (acquired != 1) {
    if (acquired < 0) ku_net_atomic_flag_set(&client->poison_requested);
    return (KuResult_bytes){ false, (KuBytes){0}, acquired == 0
      ? ku_net_error("read_timeout", "net read timed out")
      : ku_net_error("sync_error", "net client synchronization failed") };
  }
  if (ku_net_atomic_flag_load(&client->poison_requested)
      || client->socket_value == KU_NET_INVALID_SOCKET) {
    ku_net_poison(client);
    if (ku_net_gate_release(&client->gate) != 0) {
      return (KuResult_bytes){ false, (KuBytes){0},
        ku_net_error("sync_error", "net client synchronization failed") };
    }
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_net_error("client_closed", "net client is closed") };
  }
  KuBytes result = (KuBytes){0};
  result.capacity = (size_t)requested;
#if defined(KU_NATIVE_TLS_ENABLED)
  if (client->tls_enabled && result.capacity > KU_TLS_MAX_IO_BYTES)
    result.capacity = KU_TLS_MAX_IO_BYTES;
#endif
  result.ptr = (uint8_t*)malloc(result.capacity);
  if (!result.ptr) {
    if (ku_net_gate_release(&client->gate) != 0) {
      ku_net_atomic_flag_set(&client->poison_requested);
      return (KuResult_bytes){ false, (KuBytes){0},
        ku_net_error("sync_error", "net client synchronization failed") };
    }
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_net_error("out_of_memory", "net read allocation failed") };
  }
  result.storage = KU_BYTES_OWNED;
  KuError error = (KuError){0};
#if defined(KU_NATIVE_TLS_ENABLED)
  if (client->tls_enabled) {
    size_t amount = 0;
    if (client->tls_session
        && ku_net_tls_read_until(client, result.ptr, result.capacity,
            deadline, &amount, &error)) {
      result.len = amount;
    } else {
      if (error.code.len == 0)
        error = ku_net_error("tls_error", "native TLS session is unavailable");
      ku_net_poison(client);
    }
  } else {
#endif
  for (;;) {
    int waited = ku_net_socket_wait(client->socket_value, 0, deadline);
    if (waited != 1) {
      error = waited == 0
        ? ku_net_error("read_timeout", "net read timed out")
        : ku_net_error("transport_error", "net read readiness failed");
      ku_net_poison(client); break;
    }
    int amount = ku_net_socket_recv(client->socket_value, result.ptr,
        result.capacity);
    if (amount > 0) { result.len = (size_t)amount; break; }
    if (amount == 0) {
      error = ku_net_error("end_of_stream", "net peer closed the stream");
      ku_net_poison(client); break;
    }
    int socket_error = ku_net_socket_last_error();
    if (ku_net_socket_error_interrupted(socket_error)
        || ku_net_socket_error_would_block(socket_error)) continue;
    error = ku_net_error("transport_error", "net read failed");
    ku_net_poison(client); break;
  }
#if defined(KU_NATIVE_TLS_ENABLED)
  }
#endif
  if (error.code.len == 0 && ku_net_now_ms() >= deadline) {
    error = ku_net_error("read_timeout", "net read timed out");
    ku_net_poison(client);
  }
  if (ku_net_gate_release(&client->gate) != 0) {
    ku_net_atomic_flag_set(&client->poison_requested);
    if (error.code.len == 0)
      error = ku_net_error("sync_error", "net client synchronization failed");
  }
  if (error.code.len != 0) {
    ku_drop_bytes(&result);
    return (KuResult_bytes){ false, (KuBytes){0}, error };
  }
  return (KuResult_bytes){ true, result, (KuError){0} };
}

static uint8_t ku_net_close(KuNetClient* client) {
  if (!client) return 0;
  /* Exclusive-owner boundary: callers must join all read/write users before
     close. Ku's Owned handle rules enforce this path; raw generated-C callers
     must provide the same lifetime guarantee because close consumes and frees
     the handle. */
#if defined(KU_NATIVE_TLS_ENABLED)
  /* Queue, drain once, and attempt one nonblocking send. Shutdown never waits
     and unconditional session/socket release follows even after partial send. */
  if (client->tls_session && client->socket_value != KU_NET_INVALID_SOCKET) {
    uint32_t status = ku_tls_v1_client_send_close_notify(client->tls_session);
    if (status == KU_TLS_STATUS_OK) {
      uint32_t wants_write = 0;
      status = ku_tls_v1_client_wants_write(client->tls_session, &wants_write);
      if (status == KU_TLS_STATUS_OK && wants_write) {
        uint8_t ciphertext[KU_TLS_MAX_IO_BYTES];
        size_t written = 0;
        status = ku_tls_v1_client_drain_ciphertext(client->tls_session,
            ciphertext, sizeof(ciphertext), &written);
        if (status == KU_TLS_STATUS_OK && written > 0
            && written <= sizeof(ciphertext))
          (void)ku_net_socket_send(client->socket_value, ciphertext, written);
      }
    }
  }
#endif
  ku_net_poison(client);
  ku_net_gate_destroy(&client->gate);
  free(client);
  return 0;
}

static KuNetClient* ku_move_net_client(KuNetClient** value) {
  KuNetClient* moved = value ? *value : NULL;
  if (value) *value = NULL;
  return moved;
}

static KuNetClient* ku_clone_net_client(KuNetClient* value) {
  (void)value;
  fputs("net client handles cannot be cloned\n", stderr);
  exit(1);
}

static void ku_drop_net_client(KuNetClient** value) {
  if (!value || !*value) return;
  ku_net_close(*value);
  *value = NULL;
}

"#,
    );
}

/// True when the program uses a `redis` connection handle (needs the RESP runtime).
fn program_uses_redis(program: &IrProgram) -> bool {
    fn ty(t: &IrType) -> bool {
        match t {
            IrType::Named(name) => name == "__ku_redis_client",
            IrType::Array(i) | IrType::Result(i) | IrType::Cell(i) => ty(i),
            IrType::Closure { params, ret, .. } => params.iter().any(ty) || ty(ret),
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

/// Forward-declare the opaque pooled client before the Result ABI, since
/// `KuResult_redis_client` embeds a `KuRedisClient*`.
fn emit_redis_types(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_redis(program) {
        return;
    }
    out.push_str(concat!(
        "typedef struct KuRedisClient KuRedisClient;\n",
        "static KuRedisClient* ku_move_redis_client(KuRedisClient** p);\n",
        "static void ku_drop_redis_client(KuRedisClient** p);\n",
        "static KuRedisClient* ku_clone_redis_client(KuRedisClient* c);\n\n",
    ));
}

/// Emit the `redis` runtime: a bounded RESP2 client over the platform socket API
/// (Winsock on Windows, POSIX sockets on Linux/macOS; no external library).
/// The parser is deliberately strict: malformed framing, oversized values and
/// transport failures poison the connection so a later command cannot consume a
/// stale partial reply. Redis `-ERR` replies are application errors and keep the
/// connection usable.
fn emit_redis_runtime(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_redis(program) {
        return;
    }
    out.push_str(r#"
#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1

#define KU_REDIS_DEFAULT_TIMEOUT_MS 5000
#define KU_REDIS_MAX_TIMEOUT_MS 300000
#define KU_REDIS_DEFAULT_MAX_CONNECTIONS 8
#define KU_REDIS_MAX_CONNECTIONS 256
#define KU_REDIS_DEFAULT_MAX_WAITERS 64
#define KU_REDIS_MAX_WAITERS 4096
#define KU_REDIS_MAX_LINE_BYTES 4096
#define KU_REDIS_MAX_CONFIG_BYTES 65536ULL
#define KU_REDIS_MAX_BULK_BYTES (64ULL * 1024ULL * 1024ULL)
#define KU_REDIS_MAX_COMMAND_BYTES (128ULL * 1024ULL * 1024ULL)
#define KU_REDIS_SOCKET_CHUNK 2147483647U

#define KU_REDIS_READ_BUFFER_BYTES 8192
#define KU_REDIS_IO_TRANSPORT (-1)
#define KU_REDIS_IO_TIMEOUT (-4)

/* Keep all OS differences below this private transport boundary. RESP framing,
   poison rules and the public Ku API remain identical on every target. */
#if defined(_WIN32)
typedef SOCKET KuRedisSocket;
#define KU_REDIS_INVALID_SOCKET INVALID_SOCKET
#else
typedef int KuRedisSocket;
#define KU_REDIS_INVALID_SOCKET (-1)
#endif

typedef struct {
#if defined(_WIN32)
  HANDLE semaphore;
#else
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  int available;
#endif
} KuRedisGate;

typedef struct {
#if defined(_WIN32)
  SRWLOCK mutex;
  CONDITION_VARIABLE condition;
#else
  pthread_mutex_t mutex;
  pthread_cond_t condition;
#endif
} KuRedisPoolSync;

typedef struct KuRedis {
  KuRedisSocket sock;
  KuRedisGate command_gate;
  uint32_t timeout_ms;
  unsigned long long operation_deadline;
  uint8_t read_buffer[KU_REDIS_READ_BUFFER_BYTES];
  size_t read_position;
  size_t read_length;
} KuRedis;

/* Translation-unit-private raw helpers require one unique Ku owner. Repeated
   close, use after close, and starting an operation concurrently with consuming
   close are outside the contract; there is intentionally no entrant refcount. */
struct KuRedisClient {
  KuString host;
  KuString username;
  KuString password;
  KuRedis** idle;
  KuRedisPoolSync sync;
  int64_t port;
  uint32_t max_connections;
  uint32_t max_waiters;
  uint32_t connect_timeout_ms;
  uint32_t acquire_timeout_ms;
  uint32_t command_timeout_ms;
  uint32_t total_connections;
  uint32_t borrowed;
  uint32_t waiters;
  uint32_t idle_count;
  uint32_t consecutive_connect_failures;
  unsigned long long reconnect_not_before_ms;
  uint8_t has_username;
  uint8_t has_password;
  uint8_t closing;
  uint8_t finalizing;
  uint8_t connect_in_flight;
  uint8_t backoff_timer_armed;
};

typedef struct {
  bool ok;
  KuRedis* value;
  KuError error;
} KuRedisOpenResult;

typedef struct {
  bool ok;
  KuRedis* value;
  KuError error;
} KuRedisLeaseResult;

static unsigned long long ku_redis_now_ms(void) {
  return __ku_handler_now_ms();
}

static int ku_redis_socket_last_error(void) {
#if defined(_WIN32)
  return WSAGetLastError();
#else
  return errno;
#endif
}

static int ku_redis_socket_error_interrupted(int error) {
#if defined(_WIN32)
  return error == WSAEINTR;
#else
  return error == EINTR;
#endif
}

/* Blocking sockets use SO_RCVTIMEO/SO_SNDTIMEO after connect. Windows reports
   WSAETIMEDOUT (and can report WSAEWOULDBLOCK), while POSIX commonly reports
   EAGAIN/EWOULDBLOCK and may report ETIMEDOUT. These are deterministic timeout
   outcomes, not generic transport failures. */
static int ku_redis_socket_error_timed_out(int error) {
#if defined(_WIN32)
  return error == WSAETIMEDOUT || error == WSAEWOULDBLOCK;
#else
  return error == ETIMEDOUT || error == EAGAIN || error == EWOULDBLOCK;
#endif
}

static int ku_redis_socket_error_connecting(int error) {
#if defined(_WIN32)
  return error == WSAEWOULDBLOCK || error == WSAEINPROGRESS || error == WSAEINVAL || error == WSAEINTR;
#else
  return error == EINPROGRESS || error == EALREADY || error == EWOULDBLOCK || error == EINTR;
#endif
}

static void ku_redis_socket_close(KuRedisSocket socket_value) {
  if (socket_value == KU_REDIS_INVALID_SOCKET) return;
#if defined(_WIN32)
  closesocket(socket_value);
#else
  /* Never retry close(2) after EINTR: on Linux the descriptor has already been
     released and a retry could close an unrelated, newly-reused descriptor. */
  (void)close(socket_value);
#endif
}

static int ku_redis_socket_set_blocking(KuRedisSocket socket_value, int blocking) {
#if defined(_WIN32)
  u_long mode = blocking ? 0UL : 1UL;
  return ioctlsocket(socket_value, FIONBIO, &mode) == 0 ? 0 : -1;
#else
  int flags = fcntl(socket_value, F_GETFL, 0);
  if (flags < 0) return -1;
  int wanted = blocking ? (flags & ~O_NONBLOCK) : (flags | O_NONBLOCK);
  return fcntl(socket_value, F_SETFL, wanted) == 0 ? 0 : -1;
#endif
}

static int ku_redis_socket_suppress_sigpipe(KuRedisSocket socket_value) {
#if defined(__APPLE__)
  int enabled = 1;
  return setsockopt(socket_value, SOL_SOCKET, SO_NOSIGPIPE, &enabled, sizeof(enabled)) == 0 ? 0 : -1;
#else
  (void)socket_value;
  return 0;
#endif
}

static int ku_redis_socket_connect(KuRedisSocket socket_value, const struct sockaddr* address, size_t address_len) {
#if defined(_WIN32)
  if (address_len > (size_t)INT_MAX) return SOCKET_ERROR;
  return connect(socket_value, address, (int)address_len);
#else
  if (address_len > (size_t)((socklen_t)-1)) { errno = EINVAL; return -1; }
  return connect(socket_value, address, (socklen_t)address_len);
#endif
}

/* Wait for one nonblocking connect without POSIX fd_set/FD_SETSIZE hazards.
   EINTR retries always recompute the remaining absolute deadline. */
static int ku_redis_socket_wait_writable(KuRedisSocket socket_value, unsigned long long deadline) {
  for (;;) {
    unsigned long long now = ku_redis_now_ms();
    if (now >= deadline) return 0;
    unsigned long long remaining = deadline - now;
#if defined(_WIN32)
    struct timeval wait;
    wait.tv_sec = (long)(remaining / 1000ULL);
    wait.tv_usec = (long)((remaining % 1000ULL) * 1000ULL);
    fd_set writable;
    fd_set exceptional;
    FD_ZERO(&writable);
    FD_ZERO(&exceptional);
    FD_SET(socket_value, &writable);
    FD_SET(socket_value, &exceptional);
    int selected = select(0, NULL, &writable, &exceptional, &wait);
    /* A failed nonblocking connect is reported through exceptfds on Winsock.
       Let SO_ERROR below classify it (including WSAETIMEDOUT). */
    if (selected > 0)
      return FD_ISSET(socket_value, &writable) || FD_ISSET(socket_value, &exceptional) ? 1 : -1;
#else
    int wait_ms = remaining > (unsigned long long)INT_MAX ? INT_MAX : (int)remaining;
    if (wait_ms == 0) wait_ms = 1;
    struct pollfd descriptor;
    descriptor.fd = socket_value;
    descriptor.events = POLLOUT;
    descriptor.revents = 0;
    int selected = poll(&descriptor, 1, wait_ms);
    if (selected > 0) {
      if (descriptor.revents & POLLNVAL) return -1;
      return 1; /* SO_ERROR below distinguishes success from POLLERR/POLLHUP. */
    }
#endif
    if (selected == 0) return 0;
    int wait_error = ku_redis_socket_last_error();
    if (ku_redis_socket_error_timed_out(wait_error)) return KU_REDIS_IO_TIMEOUT;
    if (!ku_redis_socket_error_interrupted(wait_error)) return KU_REDIS_IO_TRANSPORT;
  }
}

static int ku_redis_socket_pending_error(KuRedisSocket socket_value, int* out_error) {
  int socket_error = 0;
#if defined(_WIN32)
  int socket_error_len = (int)sizeof(socket_error);
  int rc = getsockopt(socket_value, SOL_SOCKET, SO_ERROR, (char*)&socket_error, &socket_error_len);
#else
  socklen_t socket_error_len = (socklen_t)sizeof(socket_error);
  int rc = getsockopt(socket_value, SOL_SOCKET, SO_ERROR, &socket_error, &socket_error_len);
#endif
  if (rc != 0) return -1;
  *out_error = socket_error;
  return 0;
}

static int ku_redis_socket_set_io_timeout(KuRedisSocket socket_value, uint32_t timeout_ms) {
#if defined(_WIN32)
  DWORD timeout = (DWORD)timeout_ms;
  return setsockopt(socket_value, SOL_SOCKET, SO_RCVTIMEO, (const char*)&timeout, sizeof(timeout)) == 0 &&
         setsockopt(socket_value, SOL_SOCKET, SO_SNDTIMEO, (const char*)&timeout, sizeof(timeout)) == 0 ? 0 : -1;
#else
  struct timeval timeout;
  timeout.tv_sec = (time_t)(timeout_ms / 1000U);
  timeout.tv_usec = (suseconds_t)((timeout_ms % 1000U) * 1000U);
  return setsockopt(socket_value, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) == 0 &&
         setsockopt(socket_value, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) == 0 ? 0 : -1;
#endif
}

static int ku_redis_socket_send(KuRedisSocket socket_value, const char* data, size_t len) {
  size_t chunk = len > KU_REDIS_SOCKET_CHUNK ? KU_REDIS_SOCKET_CHUNK : len;
#if defined(_WIN32)
  return send(socket_value, data, (int)chunk, 0);
#elif defined(__APPLE__)
  /* SO_NOSIGPIPE is installed once when the socket is created. */
  ssize_t sent = send(socket_value, data, chunk, 0);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#elif defined(MSG_NOSIGNAL)
  ssize_t sent = send(socket_value, data, chunk, MSG_NOSIGNAL);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#else
#error "std.redis POSIX transport requires MSG_NOSIGNAL or SO_NOSIGPIPE"
#endif
}

static int ku_redis_socket_recv(KuRedisSocket socket_value, char* data, size_t len) {
  size_t chunk = len > KU_REDIS_SOCKET_CHUNK ? KU_REDIS_SOCKET_CHUNK : len;
#if defined(_WIN32)
  return recv(socket_value, data, (int)chunk, 0);
#else
  ssize_t received = recv(socket_value, data, chunk, 0);
  return received > (ssize_t)INT_MAX ? INT_MAX : (int)received;
#endif
}

/* Ku `str` is UTF-8, while a RESP bulk string is an arbitrary byte sequence.
   Validate the complete payload before it crosses that type boundary. */
static int ku_redis_utf8_valid(const uint8_t* data, size_t len) {
  if (len != 0 && !data) return 0;
  size_t i = 0;
  while (i < len) {
    uint8_t c = data[i];
    if (c <= 0x7f) { i++; continue; }
    if (c >= 0xc2 && c <= 0xdf) {
      if (i + 1 >= len || (data[i + 1] & 0xc0) != 0x80) return 0;
      i += 2; continue;
    }
    if (c == 0xe0) {
      if (i + 2 >= len || data[i + 1] < 0xa0 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if ((c >= 0xe1 && c <= 0xec) || (c >= 0xee && c <= 0xef)) {
      if (i + 2 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xed) {
      if (i + 2 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x9f || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xf0) {
      if (i + 3 >= len || data[i + 1] < 0x90 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c >= 0xf1 && c <= 0xf3) {
      if (i + 3 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c == 0xf4) {
      if (i + 3 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x8f || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    return 0;
  }
  return 1;
}

/* Only pass static code/message literals here. Error recovery must not itself
   allocate, especially after a receive buffer allocation has failed. */
static KuError ku_redis_static_error(const char* code, const char* message) {
  return ku_error_make(
    ku_string_static((const uint8_t*)"redis", 5),
    ku_string_static((const uint8_t*)code, strlen(code)),
    ku_string_static((const uint8_t*)message, strlen(message)));
}

static KuError ku_redis_out_of_memory_err(void) {
  return ku_redis_static_error("out_of_memory", "redis allocation failed");
}

static KuError ku_redis_connect_timeout_err(void) {
  return ku_redis_static_error("connect_timeout", "redis connect timed out");
}

static KuError ku_redis_command_timeout_err(void) {
  return ku_redis_static_error("timeout", "redis command timed out");
}

static KuError ku_redis_acquire_timeout_err(void) {
  return ku_redis_static_error(
      "acquire_timeout", "redis client timed out waiting for a connection");
}

static KuError ku_redis_pool_busy_err(void) {
  return ku_redis_static_error("pool_busy", "redis client waiter limit reached");
}

static KuError ku_redis_client_closed_err(void) {
  return ku_redis_static_error("client_closed", "redis client is closed");
}

static KuError ku_redis_connect_error_err(void) {
  return ku_redis_static_error("connect_error", "redis connection failed");
}

static KuError ku_redis_sync_error_err(void) {
  return ku_redis_static_error("sync_error", "redis client synchronization failed");
}

static int ku_redis_error_code_is(KuError error, const char* expected) {
  size_t len = strlen(expected);
  return error.code.len == len
      && (len == 0 || (error.code.ptr && memcmp(error.code.ptr, expected, len) == 0));
}

static KuError ku_redis_invalid_config_err(const char* message) {
  return ku_redis_static_error("invalid_config", message);
}

static KuError ku_redis_invalid_utf8_err(void) {
  return ku_redis_static_error("invalid_utf8", "redis response text is not valid UTF-8");
}

static KuError ku_redis_err_n(const char* m, size_t n) {
  if (!ku_redis_utf8_valid((const uint8_t*)m, n)) return ku_redis_invalid_utf8_err();
  return ku_redis_static_error(
      "redis_error", "redis server returned an error");
}

/* Internal diagnostic literals; server-provided text uses ku_redis_err_n. */
static KuError ku_redis_err(const char* m) {
  return ku_redis_static_error("redis_error", m);
}

static KuError ku_redis_missing_key_err(void) {
  return ku_redis_static_error("key_not_found", "redis key does not exist");
}

/* Never surface an AUTH server reply verbatim: a hostile or misconfigured
   endpoint can reflect the supplied username/password in its error text. */
static KuError ku_redis_auth_failed_err(void) {
  return ku_redis_static_error("auth_failed", "redis authentication failed");
}

static void ku_redis_poison(KuRedis* r) {
  if (r && r->sock != KU_REDIS_INVALID_SOCKET) {
    ku_redis_socket_close(r->sock);
    r->sock = KU_REDIS_INVALID_SOCKET;
  }
  if (r) {
    r->operation_deadline = 0;
    r->read_position = 0;
    r->read_length = 0;
  }
}

static void ku_redis_secure_wipe_bytes(void* pointer, size_t len) {
  volatile uint8_t* bytes = (volatile uint8_t*)pointer;
  while (bytes && len) {
    *bytes++ = 0;
    len--;
  }
}

static int ku_redis_is_open(const KuRedis* r) {
  return r && r->sock != KU_REDIS_INVALID_SOCKET;
}

/* 0: alive, KU_REDIS_IO_TRANSPORT: closed/no active operation,
   KU_REDIS_IO_TIMEOUT: the absolute command deadline elapsed. */
static int ku_redis_deadline_alive(KuRedis* r) {
  if (!ku_redis_is_open(r) || r->operation_deadline == 0) {
    ku_redis_poison(r);
    return KU_REDIS_IO_TRANSPORT;
  }
  if (ku_redis_now_ms() >= r->operation_deadline) {
    ku_redis_poison(r);
    return KU_REDIS_IO_TIMEOUT;
  }
  return 0;
}

static int ku_redis_refresh_io_timeout(KuRedis* r) {
  if (!ku_redis_is_open(r) || r->operation_deadline == 0) return KU_REDIS_IO_TRANSPORT;
  unsigned long long now = ku_redis_now_ms();
  if (now >= r->operation_deadline) {
    ku_redis_poison(r);
    return KU_REDIS_IO_TIMEOUT;
  }
  unsigned long long remaining = r->operation_deadline - now;
  uint32_t timeout = (uint32_t)(remaining > (unsigned long long)r->timeout_ms ? r->timeout_ms : remaining);
  if (timeout == 0) timeout = 1;
  if (ku_redis_socket_set_io_timeout(r->sock, timeout) != 0) {
    ku_redis_poison(r);
    return KU_REDIS_IO_TRANSPORT;
  }
  return 0;
}

static int ku_redis_begin_operation(KuRedis* r, unsigned long long deadline) {
  if (!ku_redis_is_open(r)) return KU_REDIS_IO_TRANSPORT;
  r->operation_deadline = deadline;
  if (deadline == 0 || ku_redis_now_ms() >= deadline) {
    ku_redis_poison(r);
    return KU_REDIS_IO_TIMEOUT;
  }
  return ku_redis_refresh_io_timeout(r);
}

/* A command's one total deadline starts before it waits for the per-connection
   lock. This prevents a contended connection from extending the budget by a
   complete timeout for every queued caller. The handler deadline is thread-local
   and, when active, is an additional upper bound. */
static unsigned long long ku_redis_deadline_after_ms(unsigned long long timeout_ms) {
  unsigned long long now = ku_redis_now_ms();
  unsigned long long deadline = (~0ULL - now < timeout_ms)
    ? ~0ULL
    : now + timeout_ms;
  if (__ku_handler_deadline != 0 && __ku_handler_deadline < deadline)
    deadline = __ku_handler_deadline;
  return deadline;
}

static unsigned long long ku_redis_saturating_add_ms(
    unsigned long long now, unsigned long long delay) {
  return ~0ULL - now < delay ? ~0ULL : now + delay;
}

static unsigned long long ku_redis_backoff_delay_ms(
    KuRedisClient* client, unsigned long long now) {
  uint32_t failures = client->consecutive_connect_failures;
  unsigned int shift = failures > 6U ? 6U : (failures ? failures - 1U : 0U);
  unsigned long long window = 25ULL << shift;
  if (window > 1000ULL) window = 1000ULL;
  unsigned long long mixed = (unsigned long long)(uintptr_t)client
      ^ now ^ ((unsigned long long)failures * 0x9e3779b97f4a7c15ULL);
  mixed ^= mixed >> 30;
  mixed *= 0xbf58476d1ce4e5b9ULL;
  mixed ^= mixed >> 27;
  mixed *= 0x94d049bb133111ebULL;
  mixed ^= mixed >> 31;
  unsigned long long lower = (window + 1ULL) / 2ULL;
  return lower + mixed % (window - lower + 1ULL);
}

static void ku_redis_record_connect_failure_locked(
    KuRedisClient* client, unsigned long long now) {
  if (client->consecutive_connect_failures != UINT32_MAX) {
    client->consecutive_connect_failures++;
  }
  client->reconnect_not_before_ms = ku_redis_saturating_add_ms(
      now, ku_redis_backoff_delay_ms(client, now));
}

static void ku_redis_record_connect_success_locked(KuRedisClient* client) {
  client->consecutive_connect_failures = 0;
  client->reconnect_not_before_ms = 0;
}

/* 0: lock held, -1: closed, -2: deadline elapsed while waiting. A lock-wait
   timeout has not written any RESP bytes and therefore must not poison the
   connection. */
static int ku_redis_lock_until(KuRedis* r, unsigned long long deadline) {
  if (!r) return -1;
  unsigned long long now = ku_redis_now_ms();
  if (deadline == 0 || now >= deadline) return -2;
#if defined(_WIN32)
  unsigned long long remaining = deadline - now;
  DWORD wait_ms = remaining > (unsigned long long)UINT32_MAX
    ? UINT32_MAX
    : (DWORD)remaining;
  DWORD wait_result = WaitForSingleObject(r->command_gate.semaphore, wait_ms);
  if (wait_result != WAIT_OBJECT_0)
    return wait_result == WAIT_TIMEOUT ? -2 : -1;
  if (!ku_redis_is_open(r)) {
    ReleaseSemaphore(r->command_gate.semaphore, 1, NULL);
    return -1;
  }
  if (ku_redis_now_ms() >= deadline) {
    ReleaseSemaphore(r->command_gate.semaphore, 1, NULL);
    return -2;
  }
  return 0;
#else
  if (pthread_mutex_lock(&r->command_gate.mutex) != 0) return -1;
  while (!r->command_gate.available) {
    now = ku_redis_now_ms();
    if (now >= deadline) {
      pthread_mutex_unlock(&r->command_gate.mutex);
      return -2;
    }
    unsigned long long remaining = deadline - now;
    int wait_result;
#if defined(__APPLE__)
    struct timespec relative = {
      (time_t)(remaining / 1000ULL),
      (long)((remaining % 1000ULL) * 1000000ULL)
    };
    wait_result = pthread_cond_timedwait_relative_np(
      &r->command_gate.condition, &r->command_gate.mutex, &relative);
#else
    struct timespec absolute = {0};
    if (clock_gettime(CLOCK_MONOTONIC, &absolute) != 0) {
      pthread_mutex_unlock(&r->command_gate.mutex);
      return -1;
    }
    absolute.tv_sec += (time_t)(remaining / 1000ULL);
    long extra_ns = (long)((remaining % 1000ULL) * 1000000ULL);
    if (absolute.tv_nsec > 999999999L - extra_ns) {
      absolute.tv_sec++;
      absolute.tv_nsec -= 1000000000L - extra_ns;
    } else {
      absolute.tv_nsec += extra_ns;
    }
    wait_result = pthread_cond_timedwait(
      &r->command_gate.condition, &r->command_gate.mutex, &absolute);
#endif
    if (wait_result == ETIMEDOUT) {
      pthread_mutex_unlock(&r->command_gate.mutex);
      return -2;
    }
    if (wait_result != 0) {
      pthread_mutex_unlock(&r->command_gate.mutex);
      return -1;
    }
  }
  if (!ku_redis_is_open(r) || ku_redis_now_ms() >= deadline) {
    int result = ku_redis_is_open(r) ? -2 : -1;
    pthread_mutex_unlock(&r->command_gate.mutex);
    return result;
  }
  r->command_gate.available = 0;
  if (pthread_mutex_unlock(&r->command_gate.mutex) != 0) {
    fputs("redis command gate unlock failed\n", stderr);
    exit(1);
  }
  return 0;
#endif
}

static void ku_redis_unlock(KuRedis* r) {
  if (!r) {
    fputs("redis command gate release failed\n", stderr);
    exit(1);
  }
#if defined(_WIN32)
  if (!r->command_gate.semaphore || !ReleaseSemaphore(r->command_gate.semaphore, 1, NULL)) {
    fputs("redis command gate release failed\n", stderr);
    exit(1);
  }
#else
  if (pthread_mutex_lock(&r->command_gate.mutex) != 0 || r->command_gate.available) {
    fputs("redis command gate release failed\n", stderr);
    exit(1);
  }
  r->command_gate.available = 1;
  int signal_result = pthread_cond_signal(&r->command_gate.condition);
  int unlock_result = pthread_mutex_unlock(&r->command_gate.mutex);
  if (signal_result != 0 || unlock_result != 0) {
    fputs("redis command gate release failed\n", stderr);
    exit(1);
  }
#endif
}

static int ku_redis_gate_init(KuRedisGate* gate) {
#if defined(_WIN32)
  gate->semaphore = CreateSemaphoreW(NULL, 1, 1, NULL);
  return gate->semaphore ? 0 : -1;
#else
  int result = pthread_mutex_init(&gate->mutex, NULL);
  if (result != 0) return -1;
#if defined(__APPLE__)
  result = pthread_cond_init(&gate->condition, NULL);
#else
  pthread_condattr_t attributes;
  result = pthread_condattr_init(&attributes);
  if (result == 0) {
    result = pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
    if (result == 0) result = pthread_cond_init(&gate->condition, &attributes);
    pthread_condattr_destroy(&attributes);
  }
#endif
  if (result != 0) {
    pthread_mutex_destroy(&gate->mutex);
    return -1;
  }
  gate->available = 1;
  return 0;
#endif
}

static void ku_redis_gate_destroy(KuRedisGate* gate) {
#if defined(_WIN32)
  if (gate->semaphore) CloseHandle(gate->semaphore);
  gate->semaphore = NULL;
#else
  if (pthread_cond_destroy(&gate->condition) != 0) {
    fputs("redis command gate condition destroy failed\n", stderr);
    exit(1);
  }
  if (pthread_mutex_destroy(&gate->mutex) != 0) {
    fputs("redis command gate mutex destroy failed\n", stderr);
    exit(1);
  }
  gate->available = 0;
#endif
}

static int ku_redis_pool_sync_init(KuRedisPoolSync* sync) {
#if defined(_WIN32)
  InitializeSRWLock(&sync->mutex);
  InitializeConditionVariable(&sync->condition);
  return 0;
#else
  int result = pthread_mutex_init(&sync->mutex, NULL);
  if (result != 0) return -1;
#if defined(__APPLE__)
  result = pthread_cond_init(&sync->condition, NULL);
#else
  pthread_condattr_t attributes;
  result = pthread_condattr_init(&attributes);
  if (result == 0) {
    result = pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
    if (result == 0) result = pthread_cond_init(&sync->condition, &attributes);
    pthread_condattr_destroy(&attributes);
  }
#endif
  if (result != 0) pthread_mutex_destroy(&sync->mutex);
  return result == 0 ? 0 : -1;
#endif
}

static int ku_redis_pool_lock(KuRedisPoolSync* sync) {
#if defined(_WIN32)
  AcquireSRWLockExclusive(&sync->mutex);
  return 0;
#else
  if (pthread_mutex_lock(&sync->mutex) != 0) {
    fputs("redis client mutex lock failed\n", stderr);
    exit(1);
  }
  return 0;
#endif
}

static int ku_redis_pool_unlock(KuRedisPoolSync* sync) {
#if defined(_WIN32)
  ReleaseSRWLockExclusive(&sync->mutex);
  return 0;
#else
  if (pthread_mutex_unlock(&sync->mutex) != 0) {
    fputs("redis client mutex unlock failed\n", stderr);
    exit(1);
  }
  return 0;
#endif
}

/* Called with the pool lock held. 0 means signaled/spurious wake, -1 means
   synchronization failure, and -2 means timeout. */
static int ku_redis_pool_wait_until(KuRedisPoolSync* sync, unsigned long long deadline) {
  unsigned long long now = ku_redis_now_ms();
  if (now >= deadline) return -2;
  unsigned long long remaining = deadline - now;
#if defined(_WIN32)
  DWORD wait_ms = remaining > (unsigned long long)UINT32_MAX
    ? UINT32_MAX : (DWORD)remaining;
  if (wait_ms == 0) wait_ms = 1;
  if (SleepConditionVariableSRW(&sync->condition, &sync->mutex, wait_ms, 0)) return 0;
  if (GetLastError() == ERROR_TIMEOUT) return -2;
  return -1;
#else
  int result;
#if defined(__APPLE__)
  struct timespec relative = {
    (time_t)(remaining / 1000ULL),
    (long)((remaining % 1000ULL) * 1000000ULL)
  };
  result = pthread_cond_timedwait_relative_np(&sync->condition, &sync->mutex, &relative);
#else
  struct timespec absolute = {0};
  if (clock_gettime(CLOCK_MONOTONIC, &absolute) != 0) return -1;
  absolute.tv_sec += (time_t)(remaining / 1000ULL);
  long extra_ns = (long)((remaining % 1000ULL) * 1000000ULL);
  if (absolute.tv_nsec > 999999999L - extra_ns) {
    absolute.tv_sec++;
    absolute.tv_nsec -= 1000000000L - extra_ns;
  } else {
    absolute.tv_nsec += extra_ns;
  }
  result = pthread_cond_timedwait(&sync->condition, &sync->mutex, &absolute);
#endif
  if (result == 0) return 0;
  if (result == ETIMEDOUT) return -2;
  return -1;
#endif
}

static void ku_redis_pool_wake_one(KuRedisPoolSync* sync) {
#if defined(_WIN32)
  WakeConditionVariable(&sync->condition);
#else
  if (pthread_cond_signal(&sync->condition) != 0) {
    fputs("redis client condition signal failed\n", stderr);
    exit(1);
  }
#endif
}

static void ku_redis_pool_wake_all(KuRedisPoolSync* sync) {
#if defined(_WIN32)
  WakeAllConditionVariable(&sync->condition);
#else
  if (pthread_cond_broadcast(&sync->condition) != 0) {
    fputs("redis client condition broadcast failed\n", stderr);
    exit(1);
  }
#endif
}

static void ku_redis_pool_sync_destroy(KuRedisPoolSync* sync) {
#if !defined(_WIN32)
  if (pthread_cond_destroy(&sync->condition) != 0) {
    fputs("redis client condition destroy failed\n", stderr);
    exit(1);
  }
  if (pthread_mutex_destroy(&sync->mutex) != 0) {
    fputs("redis client mutex destroy failed\n", stderr);
    exit(1);
  }
#else
  (void)sync;
#endif
}

static KuError ku_redis_lock_error(int rc) {
  return rc == -2
    ? ku_redis_command_timeout_err()
    : ku_redis_err("connection is closed");
}

static void ku_redis_connection_destroy(KuRedis* connection) {
  if (!connection) return;
  ku_redis_poison(connection);
  ku_redis_secure_wipe_bytes(
      connection->read_buffer, sizeof(connection->read_buffer));
  ku_redis_gate_destroy(&connection->command_gate);
  free(connection);
}

#if defined(_WIN32)
static int ku_redis_ensure_wsa(void) {
  return ku_winsock_runtime_startup();
}
#else
static int ku_redis_ensure_wsa(void) { return ku_winsock_runtime_startup(); }
#endif

static int ku_redis_send_all(KuRedis* r, const char* data, size_t len) {
  size_t sent = 0;
  while (sent < len) {
    int refresh_rc = ku_redis_refresh_io_timeout(r);
    if (refresh_rc != 0) return refresh_rc;
    size_t remaining = len - sent;
    int n = ku_redis_socket_send(r->sock, data + sent, remaining);
    if (n < 0) {
      int send_error = ku_redis_socket_last_error();
      if (ku_redis_socket_error_interrupted(send_error)) {
        int deadline_rc = ku_redis_deadline_alive(r);
        if (deadline_rc != 0) return deadline_rc;
        continue;
      }
      int timed_out = ku_redis_socket_error_timed_out(send_error)
        || (r->operation_deadline != 0 && ku_redis_now_ms() >= r->operation_deadline);
      ku_redis_poison(r);
      return timed_out ? KU_REDIS_IO_TIMEOUT : KU_REDIS_IO_TRANSPORT;
    }
    if (n == 0) {
      int timed_out = r->operation_deadline != 0
        && ku_redis_now_ms() >= r->operation_deadline;
      ku_redis_poison(r);
      return timed_out ? KU_REDIS_IO_TIMEOUT : KU_REDIS_IO_TRANSPORT;
    }
    sent += (size_t)n;
  }
  return 0;
}

/* KU_REDIS_IO_TRANSPORT: transport error, KU_REDIS_IO_TIMEOUT: timeout,
   -2: invalid/oversized local command. Transport and timeout poison the socket. */
static int ku_redis_send_cmd(KuRedis* r, int argc, const KuString* args, unsigned long long deadline) {
  if (!ku_redis_is_open(r)) return KU_REDIS_IO_TRANSPORT;
  if (!args || argc <= 0 || argc > 1024) return -2;
  size_t total = 32;
  for (int i = 0; i < argc; i++) {
    if ((args[i].len && !args[i].ptr) || args[i].len > KU_REDIS_MAX_BULK_BYTES) return -2;
    if (total > (size_t)KU_REDIS_MAX_COMMAND_BYTES - args[i].len - 64) return -2;
    total += args[i].len + 64;
  }
  int begin_rc = ku_redis_begin_operation(r, deadline);
  if (begin_rc != 0) return begin_rc;
  char header[64];
  int n = snprintf(header, sizeof(header), "*%d\r\n", argc);
  if (n <= 0 || (size_t)n >= sizeof(header)) return -2;
  int send_rc = ku_redis_send_all(r, header, (size_t)n);
  if (send_rc != 0) return send_rc;
  for (int i = 0; i < argc; i++) {
    n = snprintf(header, sizeof(header), "$%zu\r\n", args[i].len);
    if (n <= 0 || (size_t)n >= sizeof(header)) {
      ku_redis_poison(r);
      return -1;
    }
    send_rc = ku_redis_send_all(r, header, (size_t)n);
    if (send_rc != 0) return send_rc;
    if (args[i].len) {
      send_rc = ku_redis_send_all(r, (const char*)args[i].ptr, args[i].len);
      if (send_rc != 0) return send_rc;
    }
    send_rc = ku_redis_send_all(r, "\r\n", 2);
    if (send_rc != 0) return send_rc;
  }
  return 0;
}

static int ku_redis_fill_read_buffer(KuRedis* r) {
  if (r->read_position < r->read_length) return 0;
  for (;;) {
    int refresh_rc = ku_redis_refresh_io_timeout(r);
    if (refresh_rc != 0) return refresh_rc;
    int n = ku_redis_socket_recv(r->sock, (char*)r->read_buffer, KU_REDIS_READ_BUFFER_BYTES);
    if (n < 0) {
      int receive_error = ku_redis_socket_last_error();
      if (ku_redis_socket_error_interrupted(receive_error)) {
        int deadline_rc = ku_redis_deadline_alive(r);
        if (deadline_rc != 0) return deadline_rc;
        continue;
      }
      int timed_out = ku_redis_socket_error_timed_out(receive_error)
        || (r->operation_deadline != 0 && ku_redis_now_ms() >= r->operation_deadline);
      ku_redis_poison(r);
      return timed_out ? KU_REDIS_IO_TIMEOUT : KU_REDIS_IO_TRANSPORT;
    }
    if (n == 0) {
      int timed_out = r->operation_deadline != 0
        && ku_redis_now_ms() >= r->operation_deadline;
      ku_redis_poison(r);
      return timed_out ? KU_REDIS_IO_TIMEOUT : KU_REDIS_IO_TRANSPORT;
    }
    r->read_position = 0;
    r->read_length = (size_t)n;
    return 0;
  }
}

static int ku_redis_read_exact(KuRedis* r, char* data, size_t len) {
  size_t got = 0;
  while (got < len) {
    int fill_rc = ku_redis_fill_read_buffer(r);
    if (fill_rc != 0) return fill_rc;
    size_t available = r->read_length - r->read_position;
    size_t take = len - got < available ? len - got : available;
    memcpy(data + got, r->read_buffer + r->read_position, take);
    r->read_position += take;
    got += take;
  }
  return 0;
}

/* KU_REDIS_IO_TRANSPORT: transport/EOF, KU_REDIS_IO_TIMEOUT: timeout,
   -2: invalid CRLF, -3: line exceeds the configured cap. */
static int ku_redis_read_line(KuRedis* r, char* buffer, size_t capacity, size_t* out_len) {
  size_t len = 0;
  for (;;) {
    char byte = 0;
    int read_rc = ku_redis_read_exact(r, &byte, 1);
    if (read_rc != 0) return read_rc;
    if (byte == '\r') {
      char lf = 0;
      read_rc = ku_redis_read_exact(r, &lf, 1);
      if (read_rc != 0) return read_rc;
      if (lf != '\n') {
        ku_redis_poison(r);
        return -2;
      }
      buffer[len] = 0;
      *out_len = len;
      int deadline_rc = ku_redis_deadline_alive(r);
      if (deadline_rc != 0) return deadline_rc;
      return 0;
    }
    if (byte == '\n') {
      ku_redis_poison(r);
      return -2;
    }
    if (len + 1 >= capacity) {
      ku_redis_poison(r);
      return -3;
    }
    buffer[len++] = byte;
  }
}

static KuError ku_redis_read_error(int rc) {
  if (rc == KU_REDIS_IO_TIMEOUT) return ku_redis_command_timeout_err();
  if (rc == -2) return ku_redis_err("invalid RESP CRLF framing");
  if (rc == -3) return ku_redis_err("RESP line exceeds maximum size");
  return ku_redis_err("redis read failed");
}

static int ku_redis_parse_i64(const char* text, size_t len, int64_t* out) {
  if (!text || !out || len == 0) return -1;
  size_t at = 0;
  int negative = 0;
  if (text[at] == '-') {
    negative = 1;
    at++;
    if (at == len) return -1;
  }
  uint64_t limit = negative ? ((uint64_t)INT64_MAX + UINT64_C(1)) : (uint64_t)INT64_MAX;
  uint64_t value = 0;
  for (; at < len; at++) {
    unsigned char byte = (unsigned char)text[at];
    if (byte < '0' || byte > '9') return -1;
    uint64_t digit = (uint64_t)(byte - '0');
    if (value > (limit - digit) / UINT64_C(10)) return -1;
    value = value * UINT64_C(10) + digit;
  }
  if (negative) {
    *out = value == ((uint64_t)INT64_MAX + UINT64_C(1)) ? INT64_MIN : -(int64_t)value;
  } else {
    *out = (int64_t)value;
  }
  return 0;
}

static int ku_redis_resolver_out_of_memory(int code) {
#if defined(_WIN32)
  return code == WSA_NOT_ENOUGH_MEMORY;
#elif defined(EAI_MEMORY)
  return code == EAI_MEMORY;
#else
  (void)code;
  return 0;
#endif
}

static KuRedisOpenResult ku_redis_open_connection(
    KuString host,
    int64_t port,
    int64_t timeout_ms,
    unsigned long long deadline) {
  if (port < 1 || port > 65535)
    return (KuRedisOpenResult){ false, 0, ku_redis_err("redis port must be between 1 and 65535") };
  if (timeout_ms < 1 || timeout_ms > KU_REDIS_MAX_TIMEOUT_MS)
    return (KuRedisOpenResult){ false, 0, ku_redis_err("redis timeout must be between 1 and 300000 ms") };
  if (ku_redis_now_ms() >= deadline)
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_timeout_err() };
  if (!host.ptr || host.len == 0 || host.len == SIZE_MAX || memchr(host.ptr, 0, host.len))
    return (KuRedisOpenResult){ false, 0, ku_redis_err("redis host is empty or contains NUL") };
  if (ku_redis_ensure_wsa() != 0)
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_error_err() };

  /* Validation, allocation, DNS and every address share the same absolute
     deadline, also bounded by the caller's handler budget. Synchronous DNS is
     not portably interruptible, but no attempt begins after observed expiry. */
  char* hostname = (char*)malloc(host.len + 1);
  if (!hostname) return (KuRedisOpenResult){ false, 0, ku_redis_out_of_memory_err() };
  memcpy(hostname, host.ptr, host.len);
  hostname[host.len] = '\0';
  char service[6];
  int service_len = snprintf(service, sizeof(service), "%lld", (long long)port);
  if (service_len <= 0 || (size_t)service_len >= sizeof(service)) {
    free(hostname);
    return (KuRedisOpenResult){ false, 0, ku_redis_err("invalid redis port") };
  }
  struct addrinfo hints;
  memset(&hints, 0, sizeof(hints));
  hints.ai_family = AF_UNSPEC;
  hints.ai_socktype = SOCK_STREAM;
  hints.ai_protocol = IPPROTO_TCP;
  struct addrinfo* addresses = 0;
  if (ku_redis_now_ms() >= deadline) {
    free(hostname);
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_timeout_err() };
  }
  int resolve_rc = getaddrinfo(hostname, service, &hints, &addresses);
  free(hostname);
  if (resolve_rc != 0 && ku_redis_resolver_out_of_memory(resolve_rc)) {
    if (addresses) freeaddrinfo(addresses);
    return (KuRedisOpenResult){ false, 0, ku_redis_out_of_memory_err() };
  }
  if (ku_redis_now_ms() >= deadline) {
    if (addresses) freeaddrinfo(addresses);
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_timeout_err() };
  }
  if (resolve_rc != 0 || !addresses) {
    if (addresses) freeaddrinfo(addresses);
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_error_err() };
  }

  KuRedisSocket connected = KU_REDIS_INVALID_SOCKET;
  int saw_timeout = 0;
  int deadline_expired = 0;
  size_t addresses_remaining = 0;
  for (struct addrinfo* address = addresses; address; address = address->ai_next) addresses_remaining++;
  for (struct addrinfo* address = addresses; address; address = address->ai_next) {
    size_t attempts_left = addresses_remaining;
    if (addresses_remaining > 0) addresses_remaining--;
    unsigned long long now = ku_redis_now_ms();
    if (now >= deadline) { deadline_expired = 1; break; }
    KuRedisSocket candidate = socket(address->ai_family, address->ai_socktype, address->ai_protocol);
    if (candidate == KU_REDIS_INVALID_SOCKET) continue;
    if (ku_redis_socket_suppress_sigpipe(candidate) != 0 ||
        ku_redis_socket_set_blocking(candidate, 0) != 0) {
      ku_redis_socket_close(candidate);
      continue;
    }
    int ready = 0;
    int connect_rc = ku_redis_socket_connect(candidate, address->ai_addr, (size_t)address->ai_addrlen);
    if (connect_rc == 0) {
      ready = 1;
    } else {
      int connect_error = ku_redis_socket_last_error();
      if (ku_redis_socket_error_connecting(connect_error)) {
        now = ku_redis_now_ms();
        if (now < deadline) {
          unsigned long long remaining = deadline - now;
          unsigned long long attempt = attempts_left > 1 ? remaining / attempts_left : remaining;
          if (attempt == 0) attempt = 1;
          unsigned long long attempt_deadline = now + attempt;
          int selected = ku_redis_socket_wait_writable(candidate, attempt_deadline);
          if (selected > 0) {
            int socket_error = 0;
            int pending_rc = ku_redis_socket_pending_error(candidate, &socket_error);
            if (pending_rc == 0 && socket_error == 0) {
              ready = 1;
            } else if ((pending_rc == 0 && ku_redis_socket_error_timed_out(socket_error))
                || (pending_rc != 0
                    && ku_redis_socket_error_timed_out(ku_redis_socket_last_error()))) {
              saw_timeout = 1;
            }
          } else if (selected == 0 || selected == KU_REDIS_IO_TIMEOUT) {
            saw_timeout = 1;
            if (ku_redis_now_ms() >= deadline) deadline_expired = 1;
          }
        } else {
          deadline_expired = 1;
        }
      } else if (ku_redis_socket_error_timed_out(connect_error)) {
        saw_timeout = 1;
      }
    }
    if (ku_redis_socket_set_blocking(candidate, 1) != 0) ready = 0;
    if (ready) {
      if (ku_redis_socket_set_io_timeout(candidate, (uint32_t)timeout_ms) == 0) {
        if (ku_redis_now_ms() >= deadline) {
          deadline_expired = 1;
        } else {
          connected = candidate;
          break;
        }
      }
    }
    ku_redis_socket_close(candidate);
    if (deadline_expired) break;
  }
  freeaddrinfo(addresses);
  if (ku_redis_now_ms() >= deadline) deadline_expired = 1;
  if (deadline_expired) {
    ku_redis_socket_close(connected);
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_timeout_err() };
  }
  if (connected == KU_REDIS_INVALID_SOCKET) {
    if (saw_timeout)
      return (KuRedisOpenResult){ false, 0, ku_redis_connect_timeout_err() };
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_error_err() };
  }
  KuRedis* redis = (KuRedis*)malloc(sizeof(KuRedis));
  if (!redis) {
    ku_redis_socket_close(connected);
    return (KuRedisOpenResult){ false, 0, ku_redis_out_of_memory_err() };
  }
  redis->sock = connected;
  if (ku_redis_gate_init(&redis->command_gate) != 0) {
    ku_redis_socket_close(connected);
    free(redis);
    return (KuRedisOpenResult){ false, 0, ku_redis_sync_error_err() };
  }
  redis->timeout_ms = (uint32_t)timeout_ms;
  redis->operation_deadline = 0;
  redis->read_position = 0;
  redis->read_length = 0;
  if (ku_redis_now_ms() >= deadline) {
    ku_redis_gate_destroy(&redis->command_gate);
    ku_redis_socket_close(connected);
    free(redis);
    return (KuRedisOpenResult){ false, 0, ku_redis_connect_timeout_err() };
  }
  return (KuRedisOpenResult){ true, redis, (KuError){0} };
}

static KuError ku_redis_send_error(int rc) {
  if (rc == KU_REDIS_IO_TIMEOUT) return ku_redis_command_timeout_err();
  return ku_redis_err(rc == -2 ? "redis command exceeds maximum size" : "redis send failed");
}

static KuResult_null ku_redis_simple_expected_locked(KuRedis* r, int argc, const KuString* args, const char* expected, size_t expected_len, unsigned long long deadline, int redact_server_error) {
  if (!ku_redis_is_open(r))
    return (KuResult_null){ false, 0, ku_redis_err("connection is closed") };
  int send_rc = ku_redis_send_cmd(r, argc, args, deadline);
  if (send_rc != 0) return (KuResult_null){ false, 0, ku_redis_send_error(send_rc) };
  char line[KU_REDIS_MAX_LINE_BYTES + 1];
  size_t len = 0;
  int read_rc = ku_redis_read_line(r, line, sizeof(line), &len);
  KuResult_null result;
  if (read_rc != 0) {
    result = (KuResult_null){ false, 0, ku_redis_read_error(read_rc) };
  } else if (len == 0) {
    ku_redis_poison(r);
    result = (KuResult_null){ false, 0, ku_redis_err("empty RESP reply") };
  } else if (line[0] == '+' && len == expected_len + 1
      && memcmp(line + 1, expected, expected_len) == 0) {
    result = (KuResult_null){ true, 0, (KuError){0} };
  } else if (line[0] == '-') {
    result = (KuResult_null){ false, 0,
      redact_server_error ? ku_redis_auth_failed_err()
                          : ku_redis_err_n(line + 1, len - 1) };
  } else {
    ku_redis_poison(r);
    result = (KuResult_null){ false, 0,
      ku_redis_err("unexpected RESP simple string reply") };
  }
  if (redact_server_error) {
    ku_redis_secure_wipe_bytes(line, sizeof(line));
    ku_redis_secure_wipe_bytes(r->read_buffer, sizeof(r->read_buffer));
    r->read_position = 0;
    r->read_length = 0;
  }
  return result;
}

static KuResult_null ku_redis_simple_expected(KuRedis* r, int argc, const KuString* args, const char* expected, size_t expected_len, unsigned long long deadline) {
  if (!r) return (KuResult_null){ false, 0, ku_redis_err("connection is closed") };
  int lock_rc = ku_redis_lock_until(r, deadline);
  if (lock_rc != 0) return (KuResult_null){ false, 0, ku_redis_lock_error(lock_rc) };
  KuResult_null result = ku_redis_simple_expected_locked(
      r, argc, args, expected, expected_len, deadline, 0);
  ku_redis_unlock(r);
  return result;
}

static KuResult_null ku_redis_auth_expected(
    KuRedis* r, int argc, const KuString* args,
    unsigned long long deadline) {
  if (!r) return (KuResult_null){ false, 0, ku_redis_err("connection is closed") };
  int lock_rc = ku_redis_lock_until(r, deadline);
  if (lock_rc != 0) return (KuResult_null){ false, 0, ku_redis_lock_error(lock_rc) };
  KuResult_null result = ku_redis_simple_expected_locked(
      r, argc, args, "OK", 2, deadline, 1);
  ku_redis_unlock(r);
  return result;
}

static KuResult_null ku_redis_simple(KuRedis* r, int argc, const KuString* args, unsigned long long deadline) {
  return ku_redis_simple_expected(r, argc, args, "OK", 2, deadline);
}

static KuResult_null ku_redis_connection_ping(KuRedis* r, unsigned long long deadline) {
  KuString args[1] = { ku_string_static((const uint8_t*)"PING", 4) };
  return ku_redis_simple_expected(r, 1, args, "PONG", 4, deadline);
}

static KuResult_null ku_redis_connection_auth(KuRedis* r, KuString password, unsigned long long deadline) {
  KuString args[2] = { ku_string_static((const uint8_t*)"AUTH", 4), password };
  return ku_redis_auth_expected(r, 2, args, deadline);
}

static KuResult_null ku_redis_connection_auth_user(KuRedis* r, KuString username, KuString password, unsigned long long deadline) {
  KuString args[3] = { ku_string_static((const uint8_t*)"AUTH", 4), username, password };
  return ku_redis_auth_expected(r, 3, args, deadline);
}

static KuResult_null ku_redis_connection_set(KuRedis* r, KuString key, KuString value, unsigned long long deadline) {
  KuString args[3] = { ku_string_static((const uint8_t*)"SET", 3), key, value };
  return ku_redis_simple(r, 3, args, deadline);
}

static KuResult_str ku_redis_get_locked(KuRedis* r, KuString key, unsigned long long deadline) {
  if (!ku_redis_is_open(r))
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("connection is closed") };
  KuString args[2] = { ku_string_static((const uint8_t*)"GET", 3), key };
  int send_rc = ku_redis_send_cmd(r, 2, args, deadline);
  if (send_rc != 0)
    return (KuResult_str){ false, (KuString){0}, ku_redis_send_error(send_rc) };
  char line[KU_REDIS_MAX_LINE_BYTES + 1];
  size_t line_len = 0;
  int read_rc = ku_redis_read_line(r, line, sizeof(line), &line_len);
  if (read_rc != 0)
    return (KuResult_str){ false, (KuString){0}, ku_redis_read_error(read_rc) };
  if (line_len == 0) {
    ku_redis_poison(r);
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("empty RESP reply") };
  }
  if (line[0] == '-')
    return (KuResult_str){ false, (KuString){0}, ku_redis_err_n(line + 1, line_len - 1) };
  if (line[0] != '$') {
    ku_redis_poison(r);
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("expected RESP bulk string") };
  }
  if (line_len == 3 && line[1] == '-' && line[2] == '1') {
    return (KuResult_str){ false, (KuString){0}, ku_redis_missing_key_err() };
  }
  if (line_len < 2 || line[1] == '-' || line[1] == '+') {
    ku_redis_poison(r);
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("invalid RESP bulk length") };
  }
  int64_t parsed_len = 0;
  if (ku_redis_parse_i64(line + 1, line_len - 1, &parsed_len) != 0 || parsed_len < 0) {
    ku_redis_poison(r);
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("invalid RESP bulk length") };
  }
  if ((uint64_t)parsed_len > KU_REDIS_MAX_BULK_BYTES) {
    ku_redis_poison(r);
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("RESP bulk string exceeds maximum size") };
  }
  size_t value_len = (size_t)parsed_len;
  char* buffer = value_len ? (char*)malloc(value_len) : NULL;
  if (value_len && !buffer) {
    /* The header was consumed, but its body was not. A later command must never
       interpret the abandoned payload as its own reply. */
    ku_redis_poison(r);
    return (KuResult_str){ false, (KuString){0}, ku_redis_out_of_memory_err() };
  }
  if (value_len) {
    int body_rc = ku_redis_read_exact(r, buffer, value_len);
    if (body_rc != 0) {
      free(buffer);
      return (KuResult_str){ false, (KuString){0}, ku_redis_read_error(body_rc) };
    }
  }
  char ending[2] = {0, 0};
  int ending_rc = ku_redis_read_exact(r, ending, 2);
  if (ending_rc != 0) {
    free(buffer);
    return (KuResult_str){ false, (KuString){0}, ku_redis_read_error(ending_rc) };
  }
  if (ending[0] != '\r' || ending[1] != '\n') {
    free(buffer);
    ku_redis_poison(r);
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("invalid RESP bulk terminator") };
  }
  /* recv() may return a complete final chunk just after its socket timeout has
     elapsed. Re-check the absolute operation deadline after the full frame so a
     late terminator cannot turn a timed-out command into success. */
  int reply_deadline_rc = ku_redis_deadline_alive(r);
  if (reply_deadline_rc != 0) {
    free(buffer);
    return (KuResult_str){ false, (KuString){0}, ku_redis_read_error(reply_deadline_rc) };
  }
  /* Framing is complete and the connection remains synchronized here. The
     bounded UTF-8 scan is still part of the command budget; if it crosses the
     deadline, timeout takes precedence over an invalid-text diagnosis. */
  int utf8_valid = ku_redis_utf8_valid((const uint8_t*)buffer, value_len);
  int validation_deadline_rc = ku_redis_deadline_alive(r);
  if (validation_deadline_rc != 0) {
    free(buffer);
    return (KuResult_str){ false, (KuString){0}, ku_redis_read_error(validation_deadline_rc) };
  }
  /* Invalid text is a value/type error, not a transport error, so do not poison
     an otherwise synchronized connection. */
  if (!utf8_valid) {
    free(buffer);
    return (KuResult_str){ false, (KuString){0}, ku_redis_invalid_utf8_err() };
  }
  /* Transfer the validated receive buffer instead of allocating and copying a
     second value-sized buffer (the configured bulk limit is 64 MiB). */
  KuString value = (KuString){0};
  if (value_len) value = (KuString){ (uint8_t*)buffer, value_len, value_len, KU_STRING_OWNED };
  else free(buffer);
  return (KuResult_str){ true, value, (KuError){0} };
}

static KuResult_str ku_redis_connection_get(KuRedis* r, KuString key, unsigned long long deadline) {
  if (!r)
    return (KuResult_str){ false, (KuString){0}, ku_redis_err("connection is closed") };
  int lock_rc = ku_redis_lock_until(r, deadline);
  if (lock_rc != 0)
    return (KuResult_str){ false, (KuString){0}, ku_redis_lock_error(lock_rc) };
  KuResult_str result = ku_redis_get_locked(r, key, deadline);
  ku_redis_unlock(r);
  return result;
}

static KuResult_int ku_redis_integer_locked(KuRedis* r, int argc, const KuString* args, unsigned long long deadline) {
  if (!ku_redis_is_open(r))
    return (KuResult_int){ false, 0, ku_redis_err("connection is closed") };
  int send_rc = ku_redis_send_cmd(r, argc, args, deadline);
  if (send_rc != 0) return (KuResult_int){ false, 0, ku_redis_send_error(send_rc) };
  char line[KU_REDIS_MAX_LINE_BYTES + 1];
  size_t len = 0;
  int read_rc = ku_redis_read_line(r, line, sizeof(line), &len);
  if (read_rc != 0) return (KuResult_int){ false, 0, ku_redis_read_error(read_rc) };
  if (len == 0) {
    ku_redis_poison(r);
    return (KuResult_int){ false, 0, ku_redis_err("empty RESP reply") };
  }
  if (line[0] == '-') return (KuResult_int){ false, 0, ku_redis_err_n(line + 1, len - 1) };
  int64_t value = 0;
  if (line[0] != ':' || ku_redis_parse_i64(line + 1, len - 1, &value) != 0) {
    ku_redis_poison(r);
    return (KuResult_int){ false, 0, ku_redis_err("invalid RESP integer") };
  }
  return (KuResult_int){ true, value, (KuError){0} };
}

static KuResult_int ku_redis_connection_del(KuRedis* r, KuString key, unsigned long long deadline) {
  if (!r) return (KuResult_int){ false, 0, ku_redis_err("connection is closed") };
  int lock_rc = ku_redis_lock_until(r, deadline);
  if (lock_rc != 0) return (KuResult_int){ false, 0, ku_redis_lock_error(lock_rc) };
  KuString args[2] = { ku_string_static((const uint8_t*)"DEL", 3), key };
  KuResult_int result = ku_redis_integer_locked(r, 2, args, deadline);
  if (result.ok && (result.value < 0 || result.value > 1)) {
    ku_redis_poison(r);
    result = (KuResult_int){ false, 0, ku_redis_err("invalid DEL integer reply") };
  }
  ku_redis_unlock(r);
  return result;
}

static KuResult_bool ku_redis_connection_exists(KuRedis* r, KuString key, unsigned long long deadline) {
  if (!r) return (KuResult_bool){ false, false, ku_redis_err("connection is closed") };
  int lock_rc = ku_redis_lock_until(r, deadline);
  if (lock_rc != 0) return (KuResult_bool){ false, false, ku_redis_lock_error(lock_rc) };
  KuString args[2] = { ku_string_static((const uint8_t*)"EXISTS", 6), key };
  KuResult_int result = ku_redis_integer_locked(r, 2, args, deadline);
  if (!result.ok) {
    ku_redis_unlock(r);
    return (KuResult_bool){ false, false, result.error };
  }
  if (result.value != 0 && result.value != 1) {
    ku_redis_poison(r);
    ku_redis_unlock(r);
    return (KuResult_bool){ false, false, ku_redis_err("invalid EXISTS integer reply") };
  }
  ku_redis_unlock(r);
  return (KuResult_bool){ true, result.value == 1, (KuError){0} };
}

static int ku_redis_key_is(KuString key, const char* expected) {
  size_t len = strlen(expected);
  return key.len == len && (!len || (key.ptr && memcmp(key.ptr, expected, len) == 0));
}

static int ku_redis_config_key_known(KuString key) {
  return ku_redis_key_is(key, "host")
      || ku_redis_key_is(key, "port")
      || ku_redis_key_is(key, "username")
      || ku_redis_key_is(key, "password")
      || ku_redis_key_is(key, "max_connections")
      || ku_redis_key_is(key, "max_waiters")
      || ku_redis_key_is(key, "connect_timeout_ms")
      || ku_redis_key_is(key, "acquire_timeout_ms")
      || ku_redis_key_is(key, "command_timeout_ms");
}

static KuValue* ku_redis_config_get(KuObject* config, const char* key) {
  return config
    ? ku_object_get(config, ku_string_static((const uint8_t*)key, strlen(key)))
    : NULL;
}

static int ku_redis_config_int(
    KuObject* config,
    const char* key,
    uint32_t fallback,
    uint32_t minimum,
    uint32_t maximum,
    uint32_t* out,
    KuError* error) {
  KuValue* value = ku_redis_config_get(config, key);
  if (!value) { *out = fallback; return 0; }
  if (value->tag != KU_INT) {
    *error = ku_redis_invalid_config_err("redis client integer config has the wrong type");
    return -1;
  }
  if (value->as.i < (int64_t)minimum || value->as.i > (int64_t)maximum) {
    *error = ku_redis_invalid_config_err("redis client integer config is out of range");
    return -1;
  }
  *out = (uint32_t)value->as.i;
  return 0;
}

static int ku_redis_string_try_clone_owned(KuString value, KuString* out) {
  *out = (KuString){0};
  if ((value.len && !value.ptr) || value.len == SIZE_MAX) return 0;
  size_t capacity = value.len ? value.len : 1;
  uint8_t* data = (uint8_t*)malloc(capacity);
  if (!data) return 0;
  if (value.len) memcpy(data, value.ptr, value.len);
  *out = (KuString){ data, value.len, capacity, KU_STRING_OWNED };
  return 1;
}

static unsigned long long ku_redis_earlier_deadline(
    unsigned long long left,
    unsigned long long right) {
  return left < right ? left : right;
}

static void ku_redis_secure_wipe(KuString* value);

static KuRedisOpenResult ku_redis_open_authenticated(
    KuRedisClient* client,
    unsigned long long deadline) {
  KuRedisOpenResult opened = ku_redis_open_connection(
      client->host, client->port, client->connect_timeout_ms, deadline);
  if (!opened.ok) return opened;
  if (client->has_password) {
    KuResult_null auth = client->has_username
      ? ku_redis_connection_auth_user(
          opened.value, client->username, client->password, deadline)
      : ku_redis_connection_auth(opened.value, client->password, deadline);
    if (!auth.ok) {
      ku_redis_connection_destroy(opened.value);
      if (ku_redis_error_code_is(auth.error, "auth_failed")
          || ku_redis_error_code_is(auth.error, "out_of_memory")) {
        return (KuRedisOpenResult){ false, NULL, auth.error };
      }
      int timed_out = ku_redis_error_code_is(auth.error, "timeout");
      ku_error_drop(&auth.error);
      return (KuRedisOpenResult){ false, NULL,
        timed_out ? ku_redis_connect_timeout_err() : ku_redis_connect_error_err() };
    }
  }
  opened.value->timeout_ms = client->command_timeout_ms;
  return opened;
}

static void ku_redis_secure_wipe(KuString* value) {
  if (!value || value->storage != KU_STRING_OWNED || !value->ptr) return;
  size_t count = value->capacity >= value->len ? value->capacity : value->len;
  ku_redis_secure_wipe_bytes(value->ptr, count);
}

static void ku_redis_client_free_unpublished(KuRedisClient* client, int sync_ready) {
  if (!client) return;
  ku_redis_secure_wipe(&client->username);
  ku_redis_secure_wipe(&client->password);
  for (uint32_t index = 0; index < client->idle_count; index++)
    ku_redis_connection_destroy(client->idle[index]);
  if (sync_ready) ku_redis_pool_sync_destroy(&client->sync);
  free(client->idle);
  ku_string_drop(&client->host);
  ku_string_drop(&client->username);
  ku_string_drop(&client->password);
  free(client);
}

/* Called with the pool mutex held. This arbitrates concurrent close/release
   paths while the allocation is still live; it does not make a freed raw
   pointer reusable or permit a new operation to race consuming close. */
static int ku_redis_client_take_dispose(KuRedisClient* client) {
  if (!client->closing || client->finalizing
      || client->borrowed != 0 || client->waiters != 0) return 0;
  client->finalizing = 1;
  return 1;
}

static KuResult_redis_client ku_redis_client(KuObject* config) {
  if (!config)
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis.client requires a config object") };
  for (size_t index = 0; index < config->cap; index++) {
    if (config->entries[index].used
        && !ku_redis_config_key_known(config->entries[index].key)) {
      return (KuResult_redis_client){ false, NULL,
        ku_redis_invalid_config_err("redis.client config contains an unknown field") };
    }
  }
  KuValue* host_value = ku_redis_config_get(config, "host");
  if (!host_value || host_value->tag != KU_STR)
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis.client config requires string field 'host'") };
  if (!host_value->as.s.ptr || host_value->as.s.len == 0
      || host_value->as.s.len > KU_REDIS_MAX_CONFIG_BYTES
      || memchr(host_value->as.s.ptr, 0, host_value->as.s.len))
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis client host is invalid, empty, or too large") };

  KuValue* username_value = ku_redis_config_get(config, "username");
  KuValue* password_value = ku_redis_config_get(config, "password");
  if (username_value && username_value->tag != KU_STR)
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis client username must be str") };
  if (password_value && password_value->tag != KU_STR)
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis client password must be str") };
  if (username_value && !password_value)
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis client username requires password") };
  if (username_value && (username_value->as.s.len == 0
      || username_value->as.s.len > KU_REDIS_MAX_CONFIG_BYTES
      || !username_value->as.s.ptr
      || memchr(username_value->as.s.ptr, 0, username_value->as.s.len)))
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis client username is invalid, empty, or too large") };
  if (password_value && (password_value->as.s.len > KU_REDIS_MAX_CONFIG_BYTES
      || (password_value->as.s.len && !password_value->as.s.ptr)
      || (password_value->as.s.len
          && memchr(password_value->as.s.ptr, 0, password_value->as.s.len))))
    return (KuResult_redis_client){ false, NULL,
      ku_redis_invalid_config_err("redis client password is invalid or too large") };

  KuError config_error = (KuError){0};
  uint32_t port = 6379;
  uint32_t max_connections = KU_REDIS_DEFAULT_MAX_CONNECTIONS;
  uint32_t max_waiters = KU_REDIS_DEFAULT_MAX_WAITERS;
  uint32_t connect_timeout_ms = KU_REDIS_DEFAULT_TIMEOUT_MS;
  uint32_t acquire_timeout_ms = KU_REDIS_DEFAULT_TIMEOUT_MS;
  uint32_t command_timeout_ms = KU_REDIS_DEFAULT_TIMEOUT_MS;
  if (ku_redis_config_int(config, "port", 6379, 1, 65535, &port, &config_error) != 0
      || ku_redis_config_int(config, "max_connections", KU_REDIS_DEFAULT_MAX_CONNECTIONS,
          1, KU_REDIS_MAX_CONNECTIONS, &max_connections, &config_error) != 0
      || ku_redis_config_int(config, "max_waiters", KU_REDIS_DEFAULT_MAX_WAITERS,
          0, KU_REDIS_MAX_WAITERS, &max_waiters, &config_error) != 0
      || ku_redis_config_int(config, "connect_timeout_ms", KU_REDIS_DEFAULT_TIMEOUT_MS,
          1, KU_REDIS_MAX_TIMEOUT_MS, &connect_timeout_ms, &config_error) != 0
      || ku_redis_config_int(config, "acquire_timeout_ms", KU_REDIS_DEFAULT_TIMEOUT_MS,
          1, KU_REDIS_MAX_TIMEOUT_MS, &acquire_timeout_ms, &config_error) != 0
      || ku_redis_config_int(config, "command_timeout_ms", KU_REDIS_DEFAULT_TIMEOUT_MS,
          1, KU_REDIS_MAX_TIMEOUT_MS, &command_timeout_ms, &config_error) != 0) {
    return (KuResult_redis_client){ false, NULL, config_error };
  }

  KuRedisClient* client = (KuRedisClient*)malloc(sizeof(KuRedisClient));
  if (!client)
    return (KuResult_redis_client){ false, NULL, ku_redis_out_of_memory_err() };
  memset(client, 0, sizeof(*client));
  client->port = (int64_t)port;
  client->max_connections = max_connections;
  client->max_waiters = max_waiters;
  client->connect_timeout_ms = connect_timeout_ms;
  client->acquire_timeout_ms = acquire_timeout_ms;
  client->command_timeout_ms = command_timeout_ms;
  client->has_username = username_value ? 1 : 0;
  client->has_password = password_value ? 1 : 0;
  if (!ku_redis_string_try_clone_owned(host_value->as.s, &client->host)
      || (username_value
          && !ku_redis_string_try_clone_owned(username_value->as.s, &client->username))
      || (password_value
          && !ku_redis_string_try_clone_owned(password_value->as.s, &client->password))) {
    ku_redis_client_free_unpublished(client, 0);
    return (KuResult_redis_client){ false, NULL, ku_redis_out_of_memory_err() };
  }
  if ((size_t)max_connections > SIZE_MAX / sizeof(KuRedis*)) {
    ku_redis_client_free_unpublished(client, 0);
    return (KuResult_redis_client){ false, NULL, ku_redis_out_of_memory_err() };
  }
  client->idle = (KuRedis**)malloc((size_t)max_connections * sizeof(KuRedis*));
  if (!client->idle) {
    ku_redis_client_free_unpublished(client, 0);
    return (KuResult_redis_client){ false, NULL, ku_redis_out_of_memory_err() };
  }
  memset(client->idle, 0, (size_t)max_connections * sizeof(KuRedis*));
  if (ku_redis_pool_sync_init(&client->sync) != 0) {
    ku_redis_client_free_unpublished(client, 0);
    return (KuResult_redis_client){ false, NULL, ku_redis_sync_error_err() };
  }

  /* Validate networking and AUTH before publishing the client. Further pool
     connections remain lazy, but a bad host/password cannot hide until traffic. */
  unsigned long long connect_deadline = ku_redis_deadline_after_ms(connect_timeout_ms);
  KuRedisOpenResult first = ku_redis_open_authenticated(client, connect_deadline);
  if (!first.ok) {
    KuError error = first.error;
    ku_redis_client_free_unpublished(client, 1);
    return (KuResult_redis_client){ false, NULL, error };
  }
  client->idle[0] = first.value;
  client->idle_count = 1;
  client->total_connections = 1;
  return (KuResult_redis_client){ true, client, (KuError){0} };
}

/* Called with the pool lock held. Preserve wake-one efficiency without losing
   a released slot when the signaled waiter reaches its deadline first. */
static void ku_redis_client_handoff_available_locked(KuRedisClient* client) {
  if (client->waiters != 0
      && (client->idle_count != 0
          || (!client->connect_in_flight
              && ku_redis_now_ms() >= client->reconnect_not_before_ms
              && client->total_connections < client->max_connections))) {
    ku_redis_pool_wake_one(&client->sync);
  }
}

/* Keep exactly one reconnect deadline represented by a timed waiter. The
   current waiter is still included in client->waiters at this point. */
static void ku_redis_client_release_backoff_timer_locked(
    KuRedisClient* client, int owned) {
  if (!owned) return;
  client->backoff_timer_armed = 0;
  if (client->waiters > 1) ku_redis_pool_wake_one(&client->sync);
}

static KuRedisLeaseResult ku_redis_client_acquire(
    KuRedisClient* client,
    unsigned long long command_deadline) {
  if (!client)
    return (KuRedisLeaseResult){ false, NULL, ku_redis_client_closed_err() };
  unsigned long long acquire_deadline = ku_redis_earlier_deadline(
      command_deadline,
      ku_redis_deadline_after_ms(client->acquire_timeout_ms));
  if (ku_redis_pool_lock(&client->sync) != 0) {
    return (KuRedisLeaseResult){ false, NULL, ku_redis_sync_error_err() };
  }
  int registered_waiter = 0;
  for (;;) {
    if (client->closing) {
      if (registered_waiter) { client->waiters--; ku_redis_pool_wake_all(&client->sync); }
      int dispose = ku_redis_client_take_dispose(client);
      ku_redis_pool_unlock(&client->sync);
      if (dispose) ku_redis_client_free_unpublished(client, 1);
      return (KuRedisLeaseResult){ false, NULL, ku_redis_client_closed_err() };
    }
    if (ku_redis_now_ms() >= acquire_deadline) {
      if (registered_waiter) {
        client->waiters--;
        ku_redis_client_handoff_available_locked(client);
      }
      ku_redis_pool_unlock(&client->sync);
      return (KuRedisLeaseResult){ false, NULL, ku_redis_acquire_timeout_err() };
    }
    int can_claim = registered_waiter || client->waiters == 0;
    if (can_claim && client->idle_count != 0) {
      KuRedis* connection = client->idle[--client->idle_count];
      client->idle[client->idle_count] = NULL;
      client->borrowed++;
      if (registered_waiter) client->waiters--;
      ku_redis_pool_unlock(&client->sync);
      return (KuRedisLeaseResult){ true, connection, (KuError){0} };
    }
    if (can_claim && !client->connect_in_flight
        && ku_redis_now_ms() >= client->reconnect_not_before_ms
        && client->total_connections < client->max_connections) {
      client->total_connections++;
      client->borrowed++;
      client->connect_in_flight = 1;
      if (registered_waiter) client->waiters--;
      ku_redis_pool_unlock(&client->sync);

      unsigned long long connect_budget_deadline = ku_redis_saturating_add_ms(
          ku_redis_now_ms(), client->connect_timeout_ms);
      int acquire_limited_connect = acquire_deadline <= connect_budget_deadline;
      unsigned long long connect_deadline = ku_redis_earlier_deadline(
          acquire_deadline, connect_budget_deadline);
      KuRedisOpenResult opened = ku_redis_open_authenticated(client, connect_deadline);
      if (!opened.ok && acquire_limited_connect
          && ku_redis_now_ms() >= acquire_deadline
          && ku_redis_error_code_is(opened.error, "connect_timeout")) {
        ku_error_drop(&opened.error);
        opened.error = ku_redis_acquire_timeout_err();
      }
      if (ku_redis_pool_lock(&client->sync) != 0) {
        fputs("redis client mutex lock failed after connect\n", stderr);
        exit(1);
      }
      int closed = client->closing;
      int expired = ku_redis_now_ms() >= acquire_deadline;
      client->connect_in_flight = 0;
      if (opened.ok) ku_redis_record_connect_success_locked(client);
      else ku_redis_record_connect_failure_locked(client, ku_redis_now_ms());
      if (!opened.ok || closed || expired) {
        client->total_connections--;
        client->borrowed--;
        if (client->closing) ku_redis_pool_wake_all(&client->sync);
        else ku_redis_pool_wake_one(&client->sync);
        int dispose = ku_redis_client_take_dispose(client);
        ku_redis_pool_unlock(&client->sync);
        if (opened.ok) ku_redis_connection_destroy(opened.value);
        if (dispose) ku_redis_client_free_unpublished(client, 1);
        if (!opened.ok) return (KuRedisLeaseResult){ false, NULL, opened.error };
        return (KuRedisLeaseResult){ false, NULL,
          closed ? ku_redis_client_closed_err() : ku_redis_acquire_timeout_err() };
      }
      ku_redis_pool_wake_one(&client->sync);
      ku_redis_pool_unlock(&client->sync);
      return (KuRedisLeaseResult){ true, opened.value, (KuError){0} };
    }
    if (!registered_waiter) {
      if (client->max_waiters == 0 || client->waiters >= client->max_waiters) {
        ku_redis_pool_unlock(&client->sync);
        return (KuRedisLeaseResult){ false, NULL, ku_redis_pool_busy_err() };
      }
      client->waiters++;
      registered_waiter = 1;
    }
    unsigned long long wait_deadline = acquire_deadline;
    int owns_backoff_timer = 0;
    unsigned long long now = ku_redis_now_ms();
    if (client->reconnect_not_before_ms > now
        && !client->backoff_timer_armed) {
      client->backoff_timer_armed = 1;
      owns_backoff_timer = 1;
      if (client->reconnect_not_before_ms < wait_deadline) {
        wait_deadline = client->reconnect_not_before_ms;
      }
    }
    int wait_result = ku_redis_pool_wait_until(&client->sync, wait_deadline);
    ku_redis_client_release_backoff_timer_locked(client, owns_backoff_timer);
    if (wait_result != 0) {
      now = ku_redis_now_ms();
      if (wait_result == -2 && wait_deadline < acquire_deadline
          && now < acquire_deadline) {
        continue;
      }
      client->waiters--;
      if (client->closing) ku_redis_pool_wake_all(&client->sync);
      else ku_redis_client_handoff_available_locked(client);
      int dispose = ku_redis_client_take_dispose(client);
      ku_redis_pool_unlock(&client->sync);
      if (dispose) ku_redis_client_free_unpublished(client, 1);
      return (KuRedisLeaseResult){ false, NULL,
        wait_result == -2 ? ku_redis_acquire_timeout_err()
                          : ku_redis_sync_error_err() };
    }
  }
}

static void ku_redis_client_release(KuRedisClient* client, KuRedis* connection) {
  if (!client || !connection) return;
  int reusable = ku_redis_is_open(connection);
  if (ku_redis_pool_lock(&client->sync) != 0) {
    fputs("redis client mutex lock failed during release\n", stderr);
    exit(1);
  }
  if (client->borrowed == 0 || client->total_connections == 0) {
    fputs("redis client release invariant failed\n", stderr);
    exit(1);
  }
  client->borrowed--;
  if (reusable && !client->closing && client->idle_count < client->max_connections) {
    client->idle[client->idle_count++] = connection;
    connection = NULL;
  } else {
    client->total_connections--;
  }
  if (client->closing) ku_redis_pool_wake_all(&client->sync);
  else ku_redis_pool_wake_one(&client->sync);
  int dispose = ku_redis_client_take_dispose(client);
  ku_redis_pool_unlock(&client->sync);
  ku_redis_connection_destroy(connection);
  if (dispose) ku_redis_client_free_unpublished(client, 1);
}

static unsigned long long ku_redis_client_command_deadline(KuRedisClient* client) {
  return client ? ku_redis_deadline_after_ms(client->command_timeout_ms) : 0;
}

static KuResult_null ku_redis_ping(KuRedisClient* client) {
  if (!client) return (KuResult_null){ false, 0, ku_redis_client_closed_err() };
  unsigned long long deadline = ku_redis_client_command_deadline(client);
  if (!deadline || ku_redis_now_ms() >= deadline)
    return (KuResult_null){ false, 0, ku_redis_command_timeout_err() };
  KuRedisLeaseResult lease = ku_redis_client_acquire(client, deadline);
  if (!lease.ok) return (KuResult_null){ false, 0, lease.error };
  KuResult_null result = ku_redis_connection_ping(lease.value, deadline);
  ku_redis_client_release(client, lease.value);
  return result;
}

static KuResult_null ku_redis_set(KuRedisClient* client, KuString key, KuString value) {
  if (!client) return (KuResult_null){ false, 0, ku_redis_client_closed_err() };
  unsigned long long deadline = ku_redis_client_command_deadline(client);
  if (!deadline || ku_redis_now_ms() >= deadline)
    return (KuResult_null){ false, 0, ku_redis_command_timeout_err() };
  KuRedisLeaseResult lease = ku_redis_client_acquire(client, deadline);
  if (!lease.ok) return (KuResult_null){ false, 0, lease.error };
  KuResult_null result = ku_redis_connection_set(lease.value, key, value, deadline);
  ku_redis_client_release(client, lease.value);
  return result;
}

static KuResult_str ku_redis_get(KuRedisClient* client, KuString key) {
  if (!client)
    return (KuResult_str){ false, (KuString){0}, ku_redis_client_closed_err() };
  unsigned long long deadline = ku_redis_client_command_deadline(client);
  if (!deadline || ku_redis_now_ms() >= deadline)
    return (KuResult_str){ false, (KuString){0}, ku_redis_command_timeout_err() };
  KuRedisLeaseResult lease = ku_redis_client_acquire(client, deadline);
  if (!lease.ok) return (KuResult_str){ false, (KuString){0}, lease.error };
  KuResult_str result = ku_redis_connection_get(lease.value, key, deadline);
  ku_redis_client_release(client, lease.value);
  return result;
}

static KuResult_int ku_redis_del(KuRedisClient* client, KuString key) {
  if (!client) return (KuResult_int){ false, 0, ku_redis_client_closed_err() };
  unsigned long long deadline = ku_redis_client_command_deadline(client);
  if (!deadline || ku_redis_now_ms() >= deadline)
    return (KuResult_int){ false, 0, ku_redis_command_timeout_err() };
  KuRedisLeaseResult lease = ku_redis_client_acquire(client, deadline);
  if (!lease.ok) return (KuResult_int){ false, 0, lease.error };
  KuResult_int result = ku_redis_connection_del(lease.value, key, deadline);
  ku_redis_client_release(client, lease.value);
  return result;
}

static KuResult_bool ku_redis_exists(KuRedisClient* client, KuString key) {
  if (!client) return (KuResult_bool){ false, false, ku_redis_client_closed_err() };
  unsigned long long deadline = ku_redis_client_command_deadline(client);
  if (!deadline || ku_redis_now_ms() >= deadline)
    return (KuResult_bool){ false, false, ku_redis_command_timeout_err() };
  KuRedisLeaseResult lease = ku_redis_client_acquire(client, deadline);
  if (!lease.ok) return (KuResult_bool){ false, false, lease.error };
  KuResult_bool result = ku_redis_connection_exists(lease.value, key, deadline);
  ku_redis_client_release(client, lease.value);
  return result;
}

static uint8_t ku_redis_close(KuRedisClient* client) {
  if (!client) return 0;
  if (ku_redis_pool_lock(&client->sync) != 0) {
    fputs("redis client mutex lock failed during close\n", stderr);
    exit(1);
  }
  client->closing = 1;
  KuRedis** detached_idle = client->idle;
  uint32_t detached_count = client->idle_count;
  client->idle = NULL;
  client->total_connections -= detached_count;
  client->idle_count = 0;
  ku_redis_pool_wake_all(&client->sync);
  int dispose = ku_redis_client_take_dispose(client);
  ku_redis_pool_unlock(&client->sync);
  for (uint32_t index = 0; index < detached_count; index++)
    ku_redis_connection_destroy(detached_idle[index]);
  free(detached_idle);
  if (dispose) ku_redis_client_free_unpublished(client, 1);
  return 0;
}

static KuRedisClient* ku_move_redis_client(KuRedisClient** client) {
  KuRedisClient* moved = client ? *client : NULL;
  if (client) *client = NULL;
  return moved;
}

static void ku_drop_redis_client(KuRedisClient** client) {
  if (client && *client) {
    ku_redis_close(*client);
    *client = NULL;
  }
}

static KuRedisClient* ku_clone_redis_client(KuRedisClient* client) {
  (void)client;
  fputs("cannot clone a redis client\n", stderr);
  exit(1);
}

"#);
}

/// True when the program uses a pooled `pg` client (needs CRITICAL_SECTION etc.).
fn program_uses_pg_client(program: &IrProgram) -> bool {
    fn ty(t: &IrType) -> bool {
        match t {
            IrType::Named(name) => name == "__ku_pg_client",
            IrType::Array(i) | IrType::Result(i) | IrType::Cell(i) => ty(i),
            IrType::Closure { params, ret, .. } => params.iter().any(ty) || ty(ret),
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

/// Forward-declare the private libpq types and public client/result handles.
fn emit_pg_types(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_pg(program) {
        return;
    }
    out.push_str(concat!(
        "typedef struct pg_conn PGconn;\ntypedef struct pg_result PGresult;\n",
        "typedef struct KuPgCell { uint32_t offset; uint32_t len; } KuPgCell;\n",
        "typedef struct KuPgResult { size_t rows, cols, cell_capacity, bytes_len, bytes_capacity; KuPgCell* cells; uint8_t* bytes; } KuPgResult;\n",
        "typedef struct KuPgClient KuPgClient;\n",
        // The Result ABI (emitted next) calls these clone/drop helpers, which are
        // defined later in `emit_pg_runtime`; forward-declare them here.
        "static KuPgResult* ku_move_pg_result(KuPgResult** p);\n",
        "static void ku_drop_pg_result(KuPgResult** p);\n",
        "static KuPgResult* ku_clone_pg_result(KuPgResult* r);\n",
        "static KuPgClient* ku_move_pg_client(KuPgClient** p);\n",
        "static void ku_drop_pg_client(KuPgClient** p);\n",
        "static KuPgClient* ku_clone_pg_client(KuPgClient* c);\n\n",
    ));
}

/// Emit the `pg` (thin libpq binding) runtime: `PQ*` prototypes, the
/// opaque-handle move/clone/drop helpers (drop closes/frees the C resource), and the
/// private query machinery. Emitted after the Result ABI so `KuResult_pg_result` exists.
/// Values come back as text (libpq text mode). Connections are pinned to UTF-8 and
/// single rows are validated before entering a bounded, independently owned result
/// table. Each `result.value` returns a fresh owned copy. libpq still buffers a complete
/// protocol message, so a single oversized row is not a process-memory hard bound.
fn emit_pg_runtime(out: &mut COutput, program: &IrProgram) {
    if out.failed() {
        return;
    }
    if !program_uses_pg(program) {
        return;
    }
    out.push_str(concat!(
        "#define KU_FEATURE_LIBPQ 1\n",
        "extern PGconn* PQconnectStartParams(const char* const*, const char* const*, int);\n",
        "extern int PQconnectPoll(PGconn*);\n",
        "extern int PQsocket(const PGconn*);\n",
        "extern int PQstatus(const PGconn*);\n",
        "extern int PQsetnonblocking(PGconn*, int);\n",
        "extern const char* PQparameterStatus(const PGconn*, const char*);\n",
        "extern void PQfinish(PGconn*);\n",
        "extern int PQsendQuery(PGconn*, const char*);\n",
        "extern int PQsendQueryParams(PGconn*, const char*, int, const void*, const char* const*, const int*, const int*, int);\n",
        "extern int PQsetSingleRowMode(PGconn*);\n",
        "extern int PQflush(PGconn*);\n",
        "extern int PQconsumeInput(PGconn*);\n",
        "extern int PQisBusy(PGconn*);\n",
        "extern PGresult* PQgetResult(PGconn*);\n",
        "extern int PQresultStatus(const PGresult*);\n",
        "extern int PQntuples(const PGresult*);\n",
        "extern int PQnfields(const PGresult*);\n",
        "extern int PQfformat(const PGresult*, int);\n",
        "extern char* PQgetvalue(const PGresult*, int, int);\n",
        "extern int PQgetisnull(const PGresult*, int, int);\n",
        "extern int PQgetlength(const PGresult*, int, int);\n",
        "extern int PQtransactionStatus(const PGconn*);\n",
        "extern void PQclear(PGresult*);\n",
        "#define KU_PGRES_EMPTY_QUERY 0\n",
        "#define KU_PGRES_COMMAND_OK 1\n",
        "#define KU_PGRES_TUPLES_OK 2\n",
        "#define KU_PGRES_COPY_OUT 3\n",
        "#define KU_PGRES_COPY_IN 4\n",
        "#define KU_PGRES_BAD_RESPONSE 5\n",
        "#define KU_PGRES_NONFATAL_ERROR 6\n",
        "#define KU_PGRES_FATAL_ERROR 7\n",
        "#define KU_PGRES_COPY_BOTH 8\n",
        "#define KU_PGRES_SINGLE_TUPLE 9\n",
        "#define KU_PG_CONNECTION_OK 0\n",
        "#define KU_PG_CONNECTION_BAD 1\n",
        "#define KU_PGRES_POLLING_FAILED 0\n",
        "#define KU_PGRES_POLLING_READING 1\n",
        "#define KU_PGRES_POLLING_WRITING 2\n",
        "#define KU_PGRES_POLLING_OK 3\n",
        "#define KU_PGRES_POLLING_ACTIVE 4\n",
        "#define KU_PQTRANS_IDLE 0\n",
        "#define KU_PQTRANS_ACTIVE 1\n",
        "#define KU_PQTRANS_UNKNOWN 4\n",
        "#ifndef KU_PG_MONOTONIC_MS\n",
        "#define KU_PG_MONOTONIC_MS() __ku_handler_now_ms()\n",
        "#endif\n",
        "#define KU_PG_DEFAULT_QUERY_TIMEOUT_MS 30000ULL\n",
        "static int ku_pg_deadline_expired(unsigned long long deadline) { return deadline != 0 && KU_PG_MONOTONIC_MS() >= deadline; }\n",
        "static int ku_pg_utf8_valid(const uint8_t* data, size_t len, unsigned long long deadline) {\n",
        "  if (len != 0 && !data) return 0;\n",
        "  size_t i = 0; size_t next_check = 0;\n",
        "  while (i < len) {\n",
        "    if (i >= next_check) { if (ku_pg_deadline_expired(deadline)) return -1; next_check = SIZE_MAX - i < 4096 ? SIZE_MAX : i + 4096; }\n",
        "    uint8_t c = data[i];\n",
        "    if (c <= 0x7f) { i++; continue; }\n",
        "    if (c >= 0xc2 && c <= 0xdf) {\n",
        "      if (i + 1 >= len || (data[i + 1] & 0xc0) != 0x80) return 0;\n",
        "      i += 2; continue;\n",
        "    }\n",
        "    if (c == 0xe0) {\n",
        "      if (i + 2 >= len || data[i + 1] < 0xa0 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80) return 0;\n",
        "      i += 3; continue;\n",
        "    }\n",
        "    if ((c >= 0xe1 && c <= 0xec) || (c >= 0xee && c <= 0xef)) {\n",
        "      if (i + 2 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80) return 0;\n",
        "      i += 3; continue;\n",
        "    }\n",
        "    if (c == 0xed) {\n",
        "      if (i + 2 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x9f || (data[i + 2] & 0xc0) != 0x80) return 0;\n",
        "      i += 3; continue;\n",
        "    }\n",
        "    if (c == 0xf0) {\n",
        "      if (i + 3 >= len || data[i + 1] < 0x90 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;\n",
        "      i += 4; continue;\n",
        "    }\n",
        "    if (c >= 0xf1 && c <= 0xf3) {\n",
        "      if (i + 3 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;\n",
        "      i += 4; continue;\n",
        "    }\n",
        "    if (c == 0xf4) {\n",
        "      if (i + 3 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x8f || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;\n",
        "      i += 4; continue;\n",
        "    }\n",
        "    return 0;\n",
        "  }\n",
        "  return ku_pg_deadline_expired(deadline) ? -1 : 1;\n",
        "}\n",
        "#define KU_PG_MAX_CONNINFO_BYTES 65536ULL\n",
        "#define KU_PG_MAX_SQL_BYTES (16ULL * 1024ULL * 1024ULL)\n",
        "static int ku_pg_connection_is_utf8(PGconn* c) {\n",
        "  const char* encoding = c ? PQparameterStatus(c, \"client_encoding\") : 0;\n",
        "  return encoding && strcmp(encoding, \"UTF8\") == 0;\n",
        "}\n",
        "#define KU_PG_MAX_RESULT_ROWS 1000000ULL\n",
        "#define KU_PG_MAX_RESULT_COLS 4096ULL\n",
        "#define KU_PG_MAX_RESULT_CELLS 1000000ULL\n",
        "#define KU_PG_MAX_RESULT_BYTES (64ULL * 1024ULL * 1024ULL)\n",
        "#define KU_PG_NULL_CELL UINT32_MAX\n",
        "#define KU_PG_MAX_PARAM_COUNT 65535ULL\n",
        "#define KU_PG_MAX_PARAM_BYTES (64ULL * 1024ULL * 1024ULL)\n",
        "static int ku_pg_string_has_nul(KuString s) { return s.len > 0 && (!s.ptr || memchr(s.ptr, 0, s.len) != 0); }\n",
        "static KuError ku_pg_static_error(const char* code, size_t code_len, const char* message, size_t message_len) {\n",
        "  return ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)code, code_len), ku_string_static((const uint8_t*)message, message_len));\n",
        "}\n",
        "static void ku_pg_wipe_secret(void* pointer, size_t len) { volatile uint8_t* bytes = (volatile uint8_t*)pointer; while (bytes && len) { *bytes++ = 0; len--; } }\n",
        "static KuError ku_pg_query_timeout_error(void) {\n",
        "  return ku_pg_static_error(\"query_timeout\", sizeof(\"query_timeout\") - 1, \"PostgreSQL query budget expired before the requested statement was sent\", sizeof(\"PostgreSQL query budget expired before the requested statement was sent\") - 1);\n",
        "}\n",
        "static int ku_pg_query_check_deadline(unsigned long long deadline, KuError* error) {\n",
        "  if (!ku_pg_deadline_expired(deadline)) return 1;\n",
        "  if (error) *error = ku_pg_query_timeout_error();\n",
        "  return 0;\n",
        "}\n",
        /* Large strings are checked/copied in bounded chunks. These checks do
           not pretend that a C allocation or a single kernel/library call can be
           forcibly preempted, but no new chunk starts after an observed expiry. */
        "static int ku_pg_string_has_nul_until(KuString value, unsigned long long deadline) {\n",
        "  if (value.len && !value.ptr) return 1;\n",
        "  for (size_t offset = 0; offset < value.len;) {\n",
        "    if (ku_pg_deadline_expired(deadline)) return -1;\n",
        "    size_t part = value.len - offset; if (part > 65536) part = 65536;\n",
        "    if (memchr(value.ptr + offset, 0, part)) return 1;\n",
        "    offset += part;\n",
        "  }\n",
        "  return ku_pg_deadline_expired(deadline) ? -1 : 0;\n",
        "}\n",
        "static int ku_pg_copy_until(char* target, const uint8_t* source, size_t len, unsigned long long deadline) {\n",
        "  for (size_t offset = 0; offset < len;) {\n",
        "    if (ku_pg_deadline_expired(deadline)) return 0;\n",
        "    size_t part = len - offset; if (part > 65536) part = 65536;\n",
        "    memcpy(target + offset, source + offset, part); offset += part;\n",
        "  }\n",
        "  return !ku_pg_deadline_expired(deadline);\n",
        "}\n",
        r#"static int ku_pg_sql_keyword_byte(uint8_t value) {
  return (value >= (uint8_t)'A' && value <= (uint8_t)'Z')
      || (value >= (uint8_t)'a' && value <= (uint8_t)'z')
      || (value >= (uint8_t)'0' && value <= (uint8_t)'9')
      || value == (uint8_t)'_';
}
static int ku_pg_sql_token_equals(
    KuString sql, size_t start, size_t len, const char* expected) {
  size_t expected_len = strlen(expected);
  if (len != expected_len) return 0;
  for (size_t index = 0; index < len; index++) {
    uint8_t value = sql.ptr[start + index];
    if (value >= (uint8_t)'A' && value <= (uint8_t)'Z') value += 32;
    if (value != (uint8_t)expected[index]) return 0;
  }
  return 1;
}
/* PostgreSQL has `--` and nestable block comments, but `#` is not a line
   comment. Return 1 for a token, 0 for end/non-keyword input, -1 for
   fail-closed syntax, and -2 when the absolute operation deadline expires.
   The caller has already capped sql.len at KU_PG_MAX_SQL_BYTES; the scanner
   additionally checks its time budget at least every 4096 input bytes. */
static int ku_pg_sql_next_top_token(
    KuString sql, size_t* cursor, size_t* start, size_t* len,
    unsigned long long deadline) {
  size_t index = *cursor;
  size_t next_deadline_check = index;
  for (;;) {
#define KU_PG_SQL_SCAN_CHECK() do { \
  if (index >= next_deadline_check) { \
    if (ku_pg_deadline_expired(deadline)) return -2; \
    next_deadline_check = SIZE_MAX - index < 4096 ? SIZE_MAX : index + 4096; \
  } \
} while (0)
    while (index < sql.len) {
      KU_PG_SQL_SCAN_CHECK();
      uint8_t value = sql.ptr[index];
      if (value != (uint8_t)' ' && value != (uint8_t)'\t'
          && value != (uint8_t)'\r' && value != (uint8_t)'\n'
          && value != (uint8_t)'\f' && value != (uint8_t)'\v') break;
      index++;
    }
    if (index == 0 && sql.len >= 3 && sql.ptr[0] == 0xef
        && sql.ptr[1] == 0xbb && sql.ptr[2] == 0xbf) {
      index = 3;
      continue;
    }
    if (index + 1 < sql.len && sql.ptr[index] == (uint8_t)'-'
        && sql.ptr[index + 1] == (uint8_t)'-') {
      index += 2;
      while (index < sql.len && sql.ptr[index] != (uint8_t)'\n'
          && sql.ptr[index] != (uint8_t)'\r') {
        KU_PG_SQL_SCAN_CHECK();
        index++;
      }
      continue;
    }
    if (index + 1 < sql.len && sql.ptr[index] == (uint8_t)'/'
        && sql.ptr[index + 1] == (uint8_t)'*') {
      size_t depth = 1;
      index += 2;
      while (index < sql.len && depth != 0) {
        KU_PG_SQL_SCAN_CHECK();
        if (index + 1 < sql.len && sql.ptr[index] == (uint8_t)'/'
            && sql.ptr[index + 1] == (uint8_t)'*') {
          if (depth == SIZE_MAX) return -1;
          depth++; index += 2; continue;
        }
        if (index + 1 < sql.len && sql.ptr[index] == (uint8_t)'*'
            && sql.ptr[index + 1] == (uint8_t)'/') {
          depth--; index += 2; continue;
        }
        index++;
      }
      if (depth != 0) return -1;
      continue;
    }
    break;
  }
  KU_PG_SQL_SCAN_CHECK();
  if (index >= sql.len) { *cursor = index; return 0; }
  if (sql.ptr[index] == (uint8_t)';') return -1;
  if (!ku_pg_sql_keyword_byte(sql.ptr[index])
      || (sql.ptr[index] >= (uint8_t)'0' && sql.ptr[index] <= (uint8_t)'9')) {
    *cursor = index;
    return 0;
  }
  *start = index;
  while (index < sql.len && ku_pg_sql_keyword_byte(sql.ptr[index])) {
    KU_PG_SQL_SCAN_CHECK();
    index++;
  }
  *len = index - *start; *cursor = index;
#undef KU_PG_SQL_SCAN_CHECK
  return 1;
}
/* This only rejects explicit transaction/session-control statements. It does
   not claim to prove arbitrary SQL session-pure: functions, procedures and
   extensions may have hidden effects. The pooled path also checks libpq's
   post-execution protocol state and resets an idle connection before reuse. */
static int ku_pg_sql_has_explicit_session_control(
    KuString sql, unsigned long long deadline) {
  size_t cursor = 0, start = 0, len = 0;
  int token = ku_pg_sql_next_top_token(
      sql, &cursor, &start, &len, deadline);
  if (token <= 0) return token;
  static const char* const forbidden[] = {
    "begin", "start", "commit", "end", "rollback", "abort",
    "savepoint", "release", "set", "reset", "discard", "declare",
    "fetch", "move", "close", "copy", "listen", "unlisten", "notify",
    "lock", "prepare", "execute", "deallocate", "load"
  };
  for (size_t index = 0;
       index < sizeof(forbidden) / sizeof(forbidden[0]); index++) {
    if (ku_pg_sql_token_equals(sql, start, len, forbidden[index])) return 1;
  }
  int create_statement = ku_pg_sql_token_equals(sql, start, len, "create");
  int drop_statement = ku_pg_sql_token_equals(sql, start, len, "drop");
  if (create_statement || drop_statement) {
    token = ku_pg_sql_next_top_token(
        sql, &cursor, &start, &len, deadline);
    if (token <= 0) return token;
    if (create_statement && ku_pg_sql_token_equals(sql, start, len, "or")) {
      token = ku_pg_sql_next_top_token(
          sql, &cursor, &start, &len, deadline);
      if (token <= 0) return token;
      if (!ku_pg_sql_token_equals(sql, start, len, "replace")) return 0;
      token = ku_pg_sql_next_top_token(
          sql, &cursor, &start, &len, deadline);
      if (token <= 0) return token;
    }
    if (create_statement
        && (ku_pg_sql_token_equals(sql, start, len, "global")
            || ku_pg_sql_token_equals(sql, start, len, "local"))) {
      token = ku_pg_sql_next_top_token(
          sql, &cursor, &start, &len, deadline);
      if (token <= 0) return token;
    }
    if (ku_pg_sql_token_equals(sql, start, len, "temp")
        || ku_pg_sql_token_equals(sql, start, len, "temporary")) return 1;
  }
  return 0;
}
"#,
        /* A libpq connection error may echo arbitrary conninfo fields, including
           passwords. Never copy or print it on an initial connection path. */
        "static KuString ku_pg_connection_failure_message(void) {\n",
        "  return ku_string_static((const uint8_t*)\"PostgreSQL connection failed\", sizeof(\"PostgreSQL connection failed\") - 1);\n",
        "}\n",
        "static KuError ku_pg_connect_failure_error(void) {\n",
        "  return ku_error_make(ku_string_static((const uint8_t*)\"pg\", 2), ku_string_static((const uint8_t*)\"connect_error\", sizeof(\"connect_error\") - 1), ku_pg_connection_failure_message());\n",
        "}\n",
        "static KuError ku_pg_out_of_memory_error(void) {\n",
        "  return ku_pg_static_error(\"out_of_memory\", sizeof(\"out_of_memory\") - 1, \"PostgreSQL allocation failed\", sizeof(\"PostgreSQL allocation failed\") - 1);\n",
        "}\n",
        "static KuError ku_pg_client_error(const char* code, size_t code_len, const char* message, size_t message_len) {\n",
        "  return ku_pg_static_error(code, code_len, message, message_len);\n",
        "}\n",
        "static int ku_pg_validate_sql_input(KuString sql, KuError* error, unsigned long long deadline) {\n",
        "  if (error) *error = (KuError){0};\n",
        "  if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "  if (sql.len == SIZE_MAX || (sql.len && !sql.ptr)) { if (error) *error = ku_pg_static_error(\"query_error\", sizeof(\"query_error\") - 1, \"SQL storage is invalid\", sizeof(\"SQL storage is invalid\") - 1); return 0; }\n",
        "  if (sql.len > KU_PG_MAX_SQL_BYTES) { if (error) *error = ku_pg_static_error(\"query_too_large\", sizeof(\"query_too_large\") - 1, \"PostgreSQL SQL text exceeds its limit\", sizeof(\"PostgreSQL SQL text exceeds its limit\") - 1); return 0; }\n",
        "  int has_nul = ku_pg_string_has_nul_until(sql, deadline);\n",
        "  if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "  if (has_nul != 0) { if (error) *error = ku_pg_static_error(\"query_error\", sizeof(\"query_error\") - 1, \"SQL contains a NUL byte\", sizeof(\"SQL contains a NUL byte\") - 1); return 0; }\n",
        "  int valid = ku_pg_utf8_valid(sql.ptr, sql.len, deadline);\n",
        "  if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "  if (valid != 1) { if (error) *error = ku_pg_static_error(\"invalid_utf8\", sizeof(\"invalid_utf8\") - 1, \"PostgreSQL SQL text is not valid UTF-8\", sizeof(\"PostgreSQL SQL text is not valid UTF-8\") - 1); return 0; }\n",
        "  return 1;\n",
        "}\n",
        "static int ku_pg_validate_query_params(KuArray_str params, size_t* total_bytes, KuError* error, unsigned long long deadline) {\n",
        "  if (total_bytes) *total_bytes = 0;\n",
        "  if (error) *error = (KuError){0};\n",
        "  if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "  size_t n = params.len;\n",
        "  if (n > KU_PG_MAX_PARAM_COUNT || n > SIZE_MAX / sizeof(char*)) {\n",
        "    if (error) *error = ku_pg_static_error(\"parameter_too_large\", sizeof(\"parameter_too_large\") - 1, \"PostgreSQL parameter count exceeds 65535\", sizeof(\"PostgreSQL parameter count exceeds 65535\") - 1);\n",
        "    return 0;\n",
        "  }\n",
        "  if (n > 0 && !params.data) {\n",
        "    if (error) *error = ku_pg_static_error(\"query_error\", sizeof(\"query_error\") - 1, \"parameter array is invalid\", sizeof(\"parameter array is invalid\") - 1);\n",
        "    return 0;\n",
        "  }\n",
        "  size_t bytes = 0;\n",
        "  for (size_t i = 0; i < n; i++) {\n",
        "    if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "    KuString value = params.data[i];\n",
        "    if (value.len > KU_PG_MAX_PARAM_BYTES - bytes) {\n",
        "      if (error) *error = ku_pg_static_error(\"parameter_too_large\", sizeof(\"parameter_too_large\") - 1, \"PostgreSQL query parameters exceed 64 MiB total UTF-8 bytes\", sizeof(\"PostgreSQL query parameters exceed 64 MiB total UTF-8 bytes\") - 1);\n",
        "      return 0;\n",
        "    }\n",
        "    if (value.len > 0 && !value.ptr) {\n",
        "      if (error) *error = ku_pg_static_error(\"query_error\", sizeof(\"query_error\") - 1, \"query parameter storage is invalid\", sizeof(\"query parameter storage is invalid\") - 1);\n",
        "      return 0;\n",
        "    }\n",
        "    int has_nul = ku_pg_string_has_nul_until(value, deadline);\n",
        "    if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "    if (has_nul != 0) {\n",
        "      if (error) *error = ku_pg_static_error(\"query_error\", sizeof(\"query_error\") - 1, \"query parameter contains a NUL byte\", sizeof(\"query parameter contains a NUL byte\") - 1);\n",
        "      return 0;\n",
        "    }\n",
        "    int valid = ku_pg_utf8_valid(value.ptr, value.len, deadline);\n",
        "    if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "    if (valid != 1) {\n",
        "      if (error) *error = ku_pg_static_error(\"invalid_utf8\", sizeof(\"invalid_utf8\") - 1, \"PostgreSQL query parameter is not valid UTF-8\", sizeof(\"PostgreSQL query parameter is not valid UTF-8\") - 1);\n",
        "      return 0;\n",
        "    }\n",
        "    bytes += value.len;\n",
        "  }\n",
        "  if (n > SIZE_MAX - bytes) {\n",
        "    if (error) *error = ku_pg_static_error(\"parameter_too_large\", sizeof(\"parameter_too_large\") - 1, \"PostgreSQL query parameter storage is too large\", sizeof(\"PostgreSQL query parameter storage is too large\") - 1);\n",
        "    return 0;\n",
        "  }\n",
        "  if (total_bytes) *total_bytes = bytes;\n",
        "  return ku_pg_query_check_deadline(deadline, error);\n",
        "}\n",
        "typedef struct { const char** values; char* storage; } KuPgPreparedParams;\n",
        "static int ku_pg_prepare_query_params(KuArray_str params, size_t total_bytes, KuPgPreparedParams* prepared, KuError* error, unsigned long long deadline) {\n",
        "  prepared->values = 0; prepared->storage = 0;\n",
        "  if (!ku_pg_query_check_deadline(deadline, error)) return 0;\n",
        "  if (params.len == 0) return 1;\n",
        "  prepared->values = (const char**)malloc(params.len * sizeof(char*));\n",
        "  prepared->storage = (char*)malloc(total_bytes + params.len);\n",
        "  if (!prepared->values || !prepared->storage) {\n",
        "    free((void*)prepared->values); free(prepared->storage); prepared->values = 0; prepared->storage = 0;\n",
        "    if (error) *error = ku_pg_static_error(\"out_of_memory\", sizeof(\"out_of_memory\") - 1, \"failed to allocate PostgreSQL parameter buffer\", sizeof(\"failed to allocate PostgreSQL parameter buffer\") - 1);\n",
        "    return 0;\n",
        "  }\n",
        "  char* cursor = prepared->storage;\n",
        "  for (size_t i = 0; i < params.len; i++) {\n",
        "    KuString value = params.data[i]; prepared->values[i] = cursor;\n",
        "    if (!ku_pg_copy_until(cursor, value.ptr, value.len, deadline)) {\n",
        "      free((void*)prepared->values); free(prepared->storage); prepared->values = 0; prepared->storage = 0;\n",
        "      if (error) *error = ku_pg_query_timeout_error(); return 0;\n",
        "    }\n",
        "    cursor[value.len] = '\\0'; cursor += value.len + 1;\n",
        "  }\n",
        "  return 1;\n",
        "}\n",
        "static void ku_pg_drop_prepared_params(KuPgPreparedParams* prepared) {\n",
        "  if (!prepared) return; free((void*)prepared->values); free(prepared->storage); prepared->values = 0; prepared->storage = 0;\n",
        "}\n",
        "#define KU_PG_CONNECT_FAILED 0\n",
        "#define KU_PG_CONNECT_OK 1\n",
        "#define KU_PG_CONNECT_TIMED_OUT 2\n",
        "#define KU_PG_CONNECT_OUT_OF_MEMORY 3\n",
        "#define KU_PG_WAIT_ACTIVE 0\n",
        "#define KU_PG_WAIT_READ 1\n",
        "#define KU_PG_WAIT_WRITE 2\n",
        "typedef struct { PGconn* conn; int outcome; } KuPgConnectAttempt;\n",
        "static unsigned long long ku_pg_deadline_after_ms(unsigned long long timeout_ms) {\n",
        "  unsigned long long now = KU_PG_MONOTONIC_MS();\n",
        "  unsigned long long deadline = ~0ULL - now < timeout_ms ? ~0ULL : now + timeout_ms;\n",
        "  if (__ku_handler_deadline != 0 && __ku_handler_deadline < deadline) deadline = __ku_handler_deadline;\n",
        "  return deadline;\n",
        "}\n",
        /* Wait for the readiness requested by PQconnectPoll or the query pump. The timeout
           is always derived from the same absolute monotonic deadline, including
           after EINTR. ACTIVE uses a one millisecond poll with no requested I/O
           event so legacy libpq states cannot create a busy loop. */
        "static int ku_pg_wait_socket_ready(int socket_fd, int direction, unsigned long long deadline) {\n",
        "  if (socket_fd < 0) return -1;\n",
        "  for (;;) {\n",
        "    unsigned long long now = KU_PG_MONOTONIC_MS();\n",
        "    if (now >= deadline) return 0;\n",
        "    unsigned long long remaining = deadline - now;\n",
        "    int timeout_ms = remaining > 2147483647ULL ? 2147483647 : (int)remaining;\n",
        "    if (direction == KU_PG_WAIT_ACTIVE && timeout_ms > 1) timeout_ms = 1;\n",
        "#if defined(_WIN32)\n",
        "    WSAPOLLFD item; memset(&item, 0, sizeof(item));\n",
        "    item.fd = (SOCKET)(uintptr_t)(unsigned int)socket_fd;\n",
        "    item.events = (short)(((direction & KU_PG_WAIT_READ) ? POLLRDNORM : 0) | ((direction & KU_PG_WAIT_WRITE) ? POLLWRNORM : 0));\n",
        "    int rc = WSAPoll(&item, 1, timeout_ms);\n",
        "    if (rc > 0) {\n",
        "      if ((item.revents & POLLNVAL) != 0) return -1;\n",
        "      int ready = 0;\n",
        "      if ((item.revents & (POLLRDNORM | POLLERR | POLLHUP)) != 0) ready |= KU_PG_WAIT_READ;\n",
        "      if ((item.revents & POLLWRNORM) != 0) ready |= KU_PG_WAIT_WRITE;\n",
        "      if (ready) return ready;\n",
        "      return -1;\n",
        "    }\n",
        "    if (rc == 0) {\n",
        "      if (direction == KU_PG_WAIT_ACTIVE) return KU_PG_MONOTONIC_MS() >= deadline ? 0 : 1;\n",
        "      continue;\n",
        "    }\n",
        "    if (WSAGetLastError() == WSAEINTR) continue;\n",
        "    return -1;\n",
        "#else\n",
        "    struct pollfd item; memset(&item, 0, sizeof(item));\n",
        "    item.fd = socket_fd;\n",
        "    item.events = (short)(((direction & KU_PG_WAIT_READ) ? POLLIN : 0) | ((direction & KU_PG_WAIT_WRITE) ? POLLOUT : 0));\n",
        "    int rc = poll(&item, 1, timeout_ms);\n",
        "    if (rc > 0) {\n",
        "      if ((item.revents & POLLNVAL) != 0) return -1;\n",
        "      int ready = 0;\n",
        "      if ((item.revents & (POLLIN | POLLERR | POLLHUP)) != 0) ready |= KU_PG_WAIT_READ;\n",
        "      if ((item.revents & POLLOUT) != 0) ready |= KU_PG_WAIT_WRITE;\n",
        "      if (ready) return ready;\n",
        "      return -1;\n",
        "    }\n",
        "    if (rc == 0) {\n",
        "      if (direction == KU_PG_WAIT_ACTIVE) return KU_PG_MONOTONIC_MS() >= deadline ? 0 : 1;\n",
        "      continue;\n",
        "    }\n",
        "    if (errno == EINTR) continue;\n",
        "    return -1;\n",
        "#endif\n",
        "  }\n",
        "}\n",
        "static int ku_pg_wait_socket(int socket_fd, int direction, unsigned long long deadline) {\n",
        "  int ready = ku_pg_wait_socket_ready(socket_fd, direction, deadline); return ready > 0 ? 1 : ready;\n",
        "}\n",
        "static KuPgConnectAttempt ku_pg_connect_until(const char* conninfo, unsigned long long deadline) {\n",
        "  KuPgConnectAttempt attempt = { 0, KU_PG_CONNECT_FAILED };\n",
        "  unsigned long long now = KU_PG_MONOTONIC_MS();\n",
        "  if (now >= deadline) { attempt.outcome = KU_PG_CONNECT_TIMED_OUT; return attempt; }\n",
        /* libpq explicitly ignores its connect_timeout option in PQconnectPoll
           mode. The outer monotonic deadline below is the sole hard I/O budget;
           do not imply that a per-host conninfo option enforces it. */
        "  const char* keywords[] = { \"dbname\", \"client_encoding\", 0 };\n",
        "  const char* values[] = { conninfo, \"UTF8\", 0 };\n",
        /* PQconnectStartParams or a later PQconnectPoll can still synchronously
           resolve DNS unless callers supply hostaddr. The deadline started before
           this call, so an overrun is detected immediately on return even though C
           cannot hard-cancel the resolver. */
        "  PGconn* h = PQconnectStartParams(keywords, values, 1);\n",
        "  now = KU_PG_MONOTONIC_MS();\n",
        /* libpq documents NULL here as failure to allocate the PGconn itself.
           The pre-call deadline check already handled an expired budget, so do
           not overwrite this exact allocation failure with a timeout. */
        "  if (!h) { attempt.outcome = KU_PG_CONNECT_OUT_OF_MEMORY; return attempt; }\n",
        "  if (now >= deadline) { PQfinish(h); attempt.outcome = KU_PG_CONNECT_TIMED_OUT; return attempt; }\n",
        "  if (PQstatus(h) == KU_PG_CONNECTION_BAD) { PQfinish(h); return attempt; }\n",
        "  int direction = KU_PG_WAIT_WRITE;\n",
        "  for (;;) {\n",
        "    int wait_result = ku_pg_wait_socket(PQsocket(h), direction, deadline);\n",
        "    if (wait_result != 1) { PQfinish(h); attempt.outcome = wait_result == 0 ? KU_PG_CONNECT_TIMED_OUT : KU_PG_CONNECT_FAILED; return attempt; }\n",
        "    if (KU_PG_MONOTONIC_MS() >= deadline) { PQfinish(h); attempt.outcome = KU_PG_CONNECT_TIMED_OUT; return attempt; }\n",
        "    int poll_status = PQconnectPoll(h);\n",
        "    if (KU_PG_MONOTONIC_MS() >= deadline) { PQfinish(h); attempt.outcome = KU_PG_CONNECT_TIMED_OUT; return attempt; }\n",
        "    if (poll_status == KU_PGRES_POLLING_OK) {\n",
        "      if (PQstatus(h) != KU_PG_CONNECTION_OK || !ku_pg_connection_is_utf8(h)) { PQfinish(h); return attempt; }\n",
        "      attempt.conn = h; attempt.outcome = KU_PG_CONNECT_OK; return attempt;\n",
        "    }\n",
        "    if (poll_status == KU_PGRES_POLLING_FAILED) { PQfinish(h); return attempt; }\n",
        "    if (poll_status == KU_PGRES_POLLING_READING) { direction = KU_PG_WAIT_READ; continue; }\n",
        "    if (poll_status == KU_PGRES_POLLING_WRITING) { direction = KU_PG_WAIT_WRITE; continue; }\n",
        "    if (poll_status == KU_PGRES_POLLING_ACTIVE) { direction = KU_PG_WAIT_ACTIVE; continue; }\n",
        "    PQfinish(h); return attempt;\n",
        "  }\n",
        "}\n",
        "static KuPgResult* ku_move_pg_result(KuPgResult** p) { KuPgResult* m = *p; *p = 0; return m; }\n",
        "static void ku_drop_pg_result(KuPgResult** p) { if (p && *p) { free((*p)->cells); free((*p)->bytes); free(*p); *p = 0; } }\n",
        "static KuPgResult* ku_clone_pg_result(KuPgResult* r) { (void)r; fprintf(stderr, \"cannot clone a pg result\\n\"); exit(1); }\n",
        r#"/* The PGconn remains owned by Ku after an error. Shutdown, rather than
   close/PQfinish, makes any ambiguous raw connection incapable of sending more
   SQL without invalidating the owner's pointer or double-closing a recycled fd.
   No timeout path retries SQL or claims that the server rolled it back. */
static void ku_pg_break_connection(PGconn* conn, int* broken) {
  if (broken) *broken = 1;
  if (!conn) return;
  int fd = PQsocket(conn);
  if (fd < 0) return;
#if defined(_WIN32)
  shutdown((SOCKET)(uintptr_t)(unsigned int)fd, SD_BOTH);
#else
  shutdown(fd, SHUT_RDWR);
#endif
}
static KuError ku_pg_query_connection_error(void) {
  return ku_pg_static_error("query_error", sizeof("query_error") - 1, "PostgreSQL query connection failed; close and reconnect", sizeof("PostgreSQL query connection failed; close and reconnect") - 1);
}
static KuError ku_pg_execution_unknown_error(void) {
  return ku_pg_static_error("execution_unknown", sizeof("execution_unknown") - 1, "PostgreSQL statement may have executed; outcome is unknown; never retry automatically; close and reconnect", sizeof("PostgreSQL statement may have executed; outcome is unknown; never retry automatically; close and reconnect") - 1);
}
static KuError ku_pg_execution_completed_without_result_error(void) {
  return ku_pg_static_error("execution_completed_without_result", sizeof("execution_completed_without_result") - 1, "PostgreSQL statement completed but its result could not be delivered; never retry automatically", sizeof("PostgreSQL statement completed but its result could not be delivered; never retry automatically") - 1);
}
/* Only the current statement is retained. The terminal PGresult carries status
   and error text; data rows live in Ku's bounded table, independent of libpq. */
typedef struct KuPgQuery { bool ok; PGresult* terminal; KuPgResult* value; int validation; KuError error; } KuPgQuery;
static void ku_pg_drop_query(KuPgQuery* query) {
  if (query->terminal) { PQclear(query->terminal); query->terminal = 0; }
  ku_drop_pg_result(&query->value); ku_error_drop(&query->error); query->validation = 1;
}
static int ku_pg_query_completed(KuPgQuery* query) {
  if (!query || !query->terminal) return 0;
  int status = PQresultStatus(query->terminal);
  return status == KU_PGRES_COMMAND_OK || status == KU_PGRES_TUPLES_OK;
}
static KuPgQuery ku_pg_query_failure(PGconn* conn, KuPgQuery* query, unsigned long long deadline, int* broken) {
  int completed = ku_pg_query_completed(query);
  ku_pg_drop_query(query);
  ku_pg_break_connection(conn, broken);
  (void)deadline;
  return (KuPgQuery){ false, 0, 0, 1, completed ? ku_pg_execution_completed_without_result_error() : ku_pg_execution_unknown_error() };
}
static KuPgQuery ku_pg_query_failure_after_result(PGconn* conn, KuPgQuery* query, PGresult* observed, unsigned long long deadline, int* broken) {
  int completed = observed && (PQresultStatus(observed) == KU_PGRES_COMMAND_OK || PQresultStatus(observed) == KU_PGRES_TUPLES_OK);
  if (observed) PQclear(observed);
  if (!completed) return ku_pg_query_failure(conn, query, deadline, broken);
  ku_pg_drop_query(query);
  ku_pg_break_connection(conn, broken);
  (void)deadline;
  return (KuPgQuery){ false, 0, 0, 1, ku_pg_execution_completed_without_result_error() };
}
static KuPgQuery ku_pg_query_allocation_failure(PGconn* conn, KuPgQuery* query, unsigned long long deadline, int* broken) {
  return ku_pg_query_failure(conn, query, deadline, broken);
}
static size_t ku_pg_result_capacity(size_t current, size_t needed, size_t initial, size_t limit) {
  if (needed > limit) return 0;
  size_t capacity = current ? current : (initial < limit ? initial : limit);
  while (capacity < needed) capacity = capacity > limit / 2 ? limit : capacity * 2;
  return capacity;
}
/* 1: appended, 0: invalid UTF8/storage, -1: result limit, -2: deadline,
   -3: allocation failure, -4: non-text field format.
   A source is one row or a zero-row terminal only.
   Validate the entire incoming row before growing or copying the aggregate. */
static int ku_pg_result_append(KuPgResult** target, PGresult* source, unsigned long long deadline) {
  if (ku_pg_deadline_expired(deadline)) return -2;
  int rows = PQntuples(source); int cols = PQnfields(source);
  if (rows < 0 || rows > 1 || cols < 0) return 0;
  uint64_t previous_rows = *target ? (uint64_t)(*target)->rows : 0;
  uint64_t previous_bytes = *target ? (uint64_t)(*target)->bytes_len : 0;
  if ((uint64_t)rows > KU_PG_MAX_RESULT_ROWS || previous_rows > KU_PG_MAX_RESULT_ROWS - (uint64_t)rows || (uint64_t)cols > KU_PG_MAX_RESULT_COLS) return -1;
  uint64_t next_rows = previous_rows + (uint64_t)rows;
  if (next_rows && (uint64_t)cols > KU_PG_MAX_RESULT_CELLS / next_rows) return -1;
  uint64_t next_cells = next_rows * (uint64_t)cols;
  if (next_cells > SIZE_MAX / sizeof(KuPgCell) || next_rows > SIZE_MAX || previous_bytes > KU_PG_MAX_RESULT_BYTES) return -1;
  if (*target && (*target)->cols != (size_t)cols) return 0;
  /* BINARY cursors can return UTF8-valid binary bytes even for simple queries.
     Check every row description, including NULL-only and zero-row results. */
  for (int col = 0; col < cols; col++) {
    if (ku_pg_deadline_expired(deadline)) return -2;
    int format = PQfformat(source, col);
    if (ku_pg_deadline_expired(deadline)) return -2;
    if (format != 0) return -4;
  }
  uint64_t next_bytes = previous_bytes;
  for (int row = 0; row < rows; row++) for (int col = 0; col < cols; col++) {
    if (ku_pg_deadline_expired(deadline)) return -2;
    if (PQgetisnull(source, row, col)) continue;
    int length = PQgetlength(source, row, col);
    if (length < 0) return 0;
    if ((uint64_t)length > KU_PG_MAX_RESULT_BYTES - next_bytes) return -1;
    next_bytes += (uint64_t)length;
    int valid = ku_pg_utf8_valid((const uint8_t*)PQgetvalue(source, row, col), (size_t)length, deadline);
    if (valid < 0) return -2;
    if (!valid) return 0;
  }
  if (next_bytes >= KU_PG_NULL_CELL || next_bytes > SIZE_MAX) return -1;
  if (ku_pg_deadline_expired(deadline)) return -2;
  if (!*target) {
    *target = (KuPgResult*)malloc(sizeof(KuPgResult));
    if (!*target) return -3;
    **target = (KuPgResult){0}; (*target)->cols = (size_t)cols;
  }
  KuPgResult* result = *target;
  if (ku_pg_deadline_expired(deadline)) return -2;
  if ((size_t)next_cells > result->cell_capacity) {
    size_t capacity = ku_pg_result_capacity(result->cell_capacity, (size_t)next_cells, 16, (size_t)KU_PG_MAX_RESULT_CELLS);
    if (!capacity || capacity > SIZE_MAX / sizeof(KuPgCell)) return -1;
    KuPgCell* cells = (KuPgCell*)realloc(result->cells, capacity * sizeof(KuPgCell));
    if (!cells) return -3;
    result->cells = cells; result->cell_capacity = capacity;
  }
  if (ku_pg_deadline_expired(deadline)) return -2;
  if ((size_t)next_bytes > result->bytes_capacity) {
    size_t capacity = ku_pg_result_capacity(result->bytes_capacity, (size_t)next_bytes, 256, (size_t)KU_PG_MAX_RESULT_BYTES);
    if (!capacity) return -1;
    uint8_t* bytes = (uint8_t*)realloc(result->bytes, capacity);
    if (!bytes) return -3;
    result->bytes = bytes; result->bytes_capacity = capacity;
  }
  size_t cell = result->rows * result->cols;
  for (int row = 0; row < rows; row++) for (int col = 0; col < cols; col++, cell++) {
    if (ku_pg_deadline_expired(deadline)) return -2;
    KuPgCell* value = &result->cells[cell]; value->offset = 0;
    if (PQgetisnull(source, row, col)) { value->len = KU_PG_NULL_CELL; continue; }
    int length = PQgetlength(source, row, col);
    if (length < 0 || (uint64_t)length > next_bytes - (uint64_t)result->bytes_len) return 0;
    value->offset = (uint32_t)result->bytes_len; value->len = (uint32_t)length;
    /* Empty and NULL cells never perform arithmetic on an absent byte arena. */
    if (length && !ku_pg_copy_until((char*)result->bytes + result->bytes_len, (const uint8_t*)PQgetvalue(source, row, col), (size_t)length, deadline)) return -2;
    result->bytes_len += (size_t)length;
  }
  result->rows = (size_t)next_rows;
  return ku_pg_deadline_expired(deadline) ? -2 : 1;
}
/* The caller has switched libpq to nonblocking mode before sending. Waiting on
   both directions while flushing is essential: unread server notices can fill
   its send buffer while it is still waiting for the rest of our query. */
static KuPgQuery ku_pg_run_query_until(PGconn* conn, const char* sql, int parameterized, int count, const char* const* values, unsigned long long deadline, int* broken) {
  KuPgQuery query = { true, 0, 0, 1, {0} };
  if (ku_pg_deadline_expired(deadline)) return (KuPgQuery){ false, 0, 0, 1, ku_pg_query_timeout_error() };
  int sent = parameterized ? PQsendQueryParams(conn, sql, count, 0, values, 0, 0, 0) : PQsendQuery(conn, sql);
  if (!sent || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
  /* Available since libpq 9.2; do not silently fall back to complete buffering.
     This must precede every other operation on the connection after send. */
  if (!PQsetSingleRowMode(conn) || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
  for (;;) {
    if (ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
    int flush = PQflush(conn);
    if (flush < 0 || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
    if (flush == 0) break;
    if (flush != 1) return ku_pg_query_failure(conn, &query, deadline, broken);
    int ready = ku_pg_wait_socket_ready(PQsocket(conn), KU_PG_WAIT_READ | KU_PG_WAIT_WRITE, deadline);
    if (ready <= 0 || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
    if ((ready & KU_PG_WAIT_READ) && (!PQconsumeInput(conn) || ku_pg_deadline_expired(deadline))) return ku_pg_query_failure(conn, &query, deadline, broken);
  }
  int partial = 0, columns = 0;
  for (;;) {
    if (ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
    /* Drain already-buffered rows before reading more. Unconditional reads per
       row can grow libpq's input buffer and repeatedly memmove its unread tail. */
    if (PQisBusy(conn)) {
      if (ku_pg_deadline_expired(deadline) || !PQconsumeInput(conn) || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
      if (PQisBusy(conn)) {
        int ready = ku_pg_wait_socket_ready(PQsocket(conn), KU_PG_WAIT_READ, deadline);
        if (ready <= 0 || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
      }
      continue;
    }
    if (ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
    /* PQgetResult can block unless PQisBusy is false, even in nonblocking mode. */
    PGresult* next = PQgetResult(conn);
    if (ku_pg_deadline_expired(deadline)) return ku_pg_query_failure_after_result(conn, &query, next, deadline, broken);
    if (!next) {
      if (partial || !query.terminal || PQstatus(conn) != KU_PG_CONNECTION_OK) return ku_pg_query_failure(conn, &query, deadline, broken);
      return query;
    }
    int status = PQresultStatus(next);
    if (ku_pg_deadline_expired(deadline)) return ku_pg_query_failure_after_result(conn, &query, next, deadline, broken);
    if (status == KU_PGRES_SINGLE_TUPLE || status == KU_PGRES_TUPLES_OK) {
      int rows = PQntuples(next), next_columns = PQnfields(next);
      if (rows != (status == KU_PGRES_SINGLE_TUPLE ? 1 : 0) || next_columns < 0 || (partial && columns != next_columns)) {
        return ku_pg_query_failure_after_result(conn, &query, next, deadline, broken);
      }
      if (!partial) { ku_pg_drop_query(&query); columns = next_columns; }
      if (query.validation == 1) {
        int validation = ku_pg_result_append(&query.value, next, deadline);
        if (validation == -2 || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure_after_result(conn, &query, next, deadline, broken);
        if (validation == -3) return ku_pg_query_failure_after_result(conn, &query, next, deadline, broken);
        if (validation != 1) { ku_drop_pg_result(&query.value); query.validation = validation; }
      }
      if (status == KU_PGRES_SINGLE_TUPLE) { partial = 1; PQclear(next); }
      else { partial = 0; query.terminal = next; }
      continue;
    }
    if ((status == KU_PGRES_COMMAND_OK || status == KU_PGRES_EMPTY_QUERY) && (partial || PQntuples(next) != 0 || PQnfields(next) != 0)) {
      return ku_pg_query_failure_after_result(conn, &query, next, deadline, broken);
    }
    if (status < KU_PGRES_EMPTY_QUERY || status > KU_PGRES_SINGLE_TUPLE) {
      return ku_pg_query_failure_after_result(conn, &query, next, deadline, broken);
    }
    /* SQL errors discard partial rows, including a deferred size/UTF8/format error.
       A new statement replaces the previous result, matching the existing API. */
    ku_pg_drop_query(&query); query.terminal = next; partial = 0;
    if (status == KU_PGRES_COMMAND_OK) {
      int validation = ku_pg_result_append(&query.value, next, deadline);
      if (validation == -2 || ku_pg_deadline_expired(deadline)) return ku_pg_query_failure(conn, &query, deadline, broken);
      if (validation == -3) return ku_pg_query_allocation_failure(conn, &query, deadline, broken);
      if (validation != 1) return ku_pg_query_failure(conn, &query, deadline, broken);
    }
    /* COPY cannot be drained with PQgetResult. The common finish path rejects it
       and shuts down the connection immediately, as it does BAD_RESPONSE. */
    if (status == KU_PGRES_COPY_IN || status == KU_PGRES_COPY_OUT || status == KU_PGRES_COPY_BOTH || status == KU_PGRES_BAD_RESPONSE)
      return query;
  }
}
/* UTF8 restoration is exactly one internal command on the original query's
   deadline, never recursive and never the synchronous PQsetClientEncoding API. */
static int ku_pg_ensure_utf8(PGconn* conn, unsigned long long deadline, int* broken) {
  if (ku_pg_deadline_expired(deadline)) return -1;
  if (ku_pg_connection_is_utf8(conn)) return 1;
  KuPgQuery restored = ku_pg_run_query_until(conn, "SET client_encoding TO 'UTF8'", 0, 0, 0, deadline, broken);
  int valid = restored.ok && restored.terminal && PQresultStatus(restored.terminal) == KU_PGRES_COMMAND_OK && ku_pg_connection_is_utf8(conn);
  ku_pg_drop_query(&restored);
  if (ku_pg_deadline_expired(deadline)) { ku_pg_break_connection(conn, broken); return -1; }
  if (!valid) ku_pg_break_connection(conn, broken);
  return valid;
}
static int ku_pg_prepare_query_connection(PGconn* conn, unsigned long long deadline, int* broken, KuError* error) {
  if (!ku_pg_query_check_deadline(deadline, error)) return 0;
  int tx = PQtransactionStatus(conn);
  if (PQstatus(conn) != KU_PG_CONNECTION_OK || tx == KU_PQTRANS_ACTIVE || tx == KU_PQTRANS_UNKNOWN) {
    ku_pg_break_connection(conn, broken); *error = ku_pg_query_connection_error(); return 0;
  }
  if (!ku_pg_query_check_deadline(deadline, error)) return 0;
  if (PQsetnonblocking(conn, 1) != 0) { ku_pg_break_connection(conn, broken); *error = ku_pg_query_connection_error(); return 0; }
  if (!ku_pg_query_check_deadline(deadline, error)) return 0;
  /* Detect a prior shutdown or server disconnect before queuing any new SQL. */
  if (!PQconsumeInput(conn) || PQstatus(conn) != KU_PG_CONNECTION_OK) { ku_pg_break_connection(conn, broken); *error = ku_pg_query_connection_error(); return 0; }
  if (!ku_pg_query_check_deadline(deadline, error)) return 0;
  int utf8 = ku_pg_ensure_utf8(conn, deadline, broken);
  if (utf8 < 0) { *error = ku_pg_query_timeout_error(); return 0; }
  if (!utf8) { *error = ku_pg_static_error("query_error", sizeof("query_error") - 1, "failed to enforce PostgreSQL UTF8 client encoding; close and reconnect", sizeof("failed to enforce PostgreSQL UTF8 client encoding; close and reconnect") - 1); return 0; }
  return ku_pg_query_check_deadline(deadline, error);
}
static KuResult_pg_result ku_pg_failed_result(KuPgQuery query) {
  KuError error = query.error; query.error = (KuError){0}; ku_pg_drop_query(&query);
  return (KuResult_pg_result){ false, 0, error };
}
static KuResult_pg_result ku_pg_finish_query(PGconn* conn, KuPgQuery query, unsigned long long deadline, int* broken) {
  if (!query.ok) return ku_pg_failed_result(query);
  if (ku_pg_deadline_expired(deadline)) return ku_pg_failed_result(ku_pg_query_failure(conn, &query, deadline, broken));
  int st = query.terminal ? PQresultStatus(query.terminal) : -1;
  if (ku_pg_deadline_expired(deadline)) return ku_pg_failed_result(ku_pg_query_failure(conn, &query, deadline, broken));
  if (st == KU_PGRES_EMPTY_QUERY) {
    ku_pg_drop_query(&query);
    return (KuResult_pg_result){ false, 0, ku_pg_static_error("query_error", sizeof("query_error") - 1, "empty SQL query is not allowed", sizeof("empty SQL query is not allowed") - 1) };
  }
  if (st == KU_PGRES_COPY_IN || st == KU_PGRES_COPY_OUT || st == KU_PGRES_COPY_BOTH) {
    ku_pg_break_connection(conn, broken); ku_pg_drop_query(&query);
    return (KuResult_pg_result){ false, 0, ku_pg_execution_unknown_error() };
  }
  if (st == KU_PGRES_BAD_RESPONSE) {
    ku_pg_break_connection(conn, broken); ku_pg_drop_query(&query);
    return (KuResult_pg_result){ false, 0, ku_pg_execution_unknown_error() };
  }
  if (PQstatus(conn) == KU_PG_CONNECTION_OK && !ku_pg_connection_is_utf8(conn)) {
    ku_pg_drop_query(&query);
    int restored = ku_pg_ensure_utf8(conn, deadline, broken);
    if (restored < 0 || ku_pg_deadline_expired(deadline)) { ku_pg_break_connection(conn, broken); return (KuResult_pg_result){ false, 0, ku_pg_execution_completed_without_result_error() }; }
    return (KuResult_pg_result){ false, 0, ku_pg_execution_completed_without_result_error() };
  }
  if (!query.terminal || (st != KU_PGRES_COMMAND_OK && st != KU_PGRES_TUPLES_OK)) {
    if (ku_pg_deadline_expired(deadline)) return ku_pg_failed_result(ku_pg_query_failure(conn, &query, deadline, broken));
    ku_pg_drop_query(&query);
    return (KuResult_pg_result){ false, 0, ku_pg_static_error("query_error", sizeof("query_error") - 1, "PostgreSQL query failed", sizeof("PostgreSQL query failed") - 1) };
  }
  if (ku_pg_deadline_expired(deadline)) return ku_pg_failed_result(ku_pg_query_failure(conn, &query, deadline, broken));
  if (query.validation == -4) {
    ku_pg_drop_query(&query);
    return (KuResult_pg_result){ false, 0, ku_pg_execution_completed_without_result_error() };
  }
  if (query.validation < 0) {
    ku_pg_drop_query(&query);
    return (KuResult_pg_result){ false, 0, ku_pg_execution_completed_without_result_error() };
  }
  if (query.validation == 0) {
    ku_pg_drop_query(&query);
    return (KuResult_pg_result){ false, 0, ku_pg_execution_completed_without_result_error() };
  }
  if (!query.value) return ku_pg_failed_result(ku_pg_query_failure(conn, &query, deadline, broken));
  KuPgResult* value = query.value; query.value = 0; ku_pg_drop_query(&query);
  if (ku_pg_deadline_expired(deadline)) { ku_drop_pg_result(&value); ku_pg_break_connection(conn, broken); return (KuResult_pg_result){ false, 0, ku_pg_execution_completed_without_result_error() }; }
  return (KuResult_pg_result){ true, value, (KuError){0} };
}
static KuResult_pg_result ku_pg_query_prepared_validated_until(PGconn* conn, KuString sql, int parameterized, int count, const char* const* values, unsigned long long deadline, int* broken) {
  if (broken) *broken = 0;
  if (!conn) return (KuResult_pg_result){ false, 0, ku_pg_static_error("query_error", sizeof("query_error") - 1, "connection is closed", sizeof("connection is closed") - 1) };
  if (ku_pg_deadline_expired(deadline)) return (KuResult_pg_result){ false, 0, ku_pg_query_timeout_error() };
  char* query = (char*)malloc(sql.len + 1);
  if (!query) return (KuResult_pg_result){ false, 0, ku_pg_static_error("out_of_memory", sizeof("out_of_memory") - 1, "failed to allocate PostgreSQL SQL buffer", sizeof("failed to allocate PostgreSQL SQL buffer") - 1) };
  if (!ku_pg_copy_until(query, sql.ptr, sql.len, deadline)) { free(query); return (KuResult_pg_result){ false, 0, ku_pg_query_timeout_error() }; }
  query[sql.len] = '\0';
  KuError prepare_error = (KuError){0};
  if (!ku_pg_prepare_query_connection(conn, deadline, broken, &prepare_error)) { free(query); return (KuResult_pg_result){ false, 0, prepare_error }; }
  KuPgQuery result = ku_pg_run_query_until(conn, query, parameterized, count, values, deadline, broken);
  free(query);
  return ku_pg_finish_query(conn, result, deadline, broken);
}
static KuResult_pg_result ku_pg_query_prepared_until(PGconn* conn, KuString sql, int parameterized, int count, const char* const* values, unsigned long long deadline, int* broken) {
  if (broken) *broken = 0;
  if (!conn) return (KuResult_pg_result){ false, 0, ku_pg_static_error("query_error", sizeof("query_error") - 1, "connection is closed", sizeof("connection is closed") - 1) };
  KuError sql_error = (KuError){0};
  if (!ku_pg_validate_sql_input(sql, &sql_error, deadline)) return (KuResult_pg_result){ false, 0, sql_error };
  return ku_pg_query_prepared_validated_until(conn, sql, parameterized, count, values, deadline, broken);
}
static KuResult_pg_result ku_pg_query_impl(PGconn* conn, KuString sql, unsigned long long deadline, int* broken) {
  return ku_pg_query_prepared_until(conn, sql, 0, 0, 0, deadline, broken);
}
static KuResult_pg_result ku_pg_query(PGconn* conn, KuString sql) {
  return ku_pg_query_impl(conn, sql, ku_pg_deadline_after_ms(KU_PG_DEFAULT_QUERY_TIMEOUT_MS), 0);
}
/* Parameters are bound by libpq's extended protocol, never interpolated into SQL. */
static KuResult_pg_result ku_pg_query_params_validated_impl(PGconn* conn, KuString sql, KuArray_str params, size_t param_bytes, unsigned long long deadline, int* broken) {
  if (broken) *broken = 0;
  KuPgPreparedParams prepared; KuError prepare_error = (KuError){0};
  if (!ku_pg_prepare_query_params(params, param_bytes, &prepared, &prepare_error, deadline)) return (KuResult_pg_result){ false, 0, prepare_error };
  KuResult_pg_result result = ku_pg_query_prepared_until(conn, sql, 1, (int)params.len, prepared.values, deadline, broken);
  ku_pg_drop_prepared_params(&prepared);
  return result;
}
static KuResult_pg_result ku_pg_query_params_all_validated_impl(PGconn* conn, KuString sql, KuArray_str params, size_t param_bytes, unsigned long long deadline, int* broken) {
  if (broken) *broken = 0;
  KuPgPreparedParams prepared; KuError prepare_error = (KuError){0};
  if (!ku_pg_prepare_query_params(params, param_bytes, &prepared, &prepare_error, deadline)) return (KuResult_pg_result){ false, 0, prepare_error };
  KuResult_pg_result result = ku_pg_query_prepared_validated_until(conn, sql, 1, (int)params.len, prepared.values, deadline, broken);
  ku_pg_drop_prepared_params(&prepared);
  return result;
}
static KuResult_pg_result ku_pg_query_params_impl(PGconn* conn, KuString sql, KuArray_str params, unsigned long long deadline, int* broken) {
  if (broken) *broken = 0;
  if (!conn) return (KuResult_pg_result){ false, 0, ku_pg_static_error("query_error", sizeof("query_error") - 1, "connection is closed", sizeof("connection is closed") - 1) };
  size_t param_bytes = 0; KuError param_error = (KuError){0};
  if (!ku_pg_validate_query_params(params, &param_bytes, &param_error, deadline)) return (KuResult_pg_result){ false, 0, param_error };
  return ku_pg_query_params_validated_impl(conn, sql, params, param_bytes, deadline, broken);
}
static KuResult_pg_result ku_pg_query_params(PGconn* conn, KuString sql, KuArray_str params) {
  return ku_pg_query_params_impl(conn, sql, params, ku_pg_deadline_after_ms(KU_PG_DEFAULT_QUERY_TIMEOUT_MS), 0);
}
"#,
        "static int64_t ku_pg_rows(KuPgResult* r) { return r ? (int64_t)r->rows : 0; }\n",
        "static int64_t ku_pg_cols(KuPgResult* r) { return r ? (int64_t)r->cols : 0; }\n",
        "static int ku_pg_cell_in_bounds(KuPgResult* r, int64_t row, int64_t col) {\n",
        "  if (!r || row < 0 || col < 0 || row > INT32_MAX || col > INT32_MAX) return 0;\n",
        "  return (uint64_t)row < (uint64_t)r->rows && (uint64_t)col < (uint64_t)r->cols;\n",
        "}\n",
        "static KuError ku_pg_value_error(void) {\n",
        "  return ku_pg_static_error(\"value_error\", sizeof(\"value_error\") - 1, \"result row or column is out of bounds\", sizeof(\"result row or column is out of bounds\") - 1);\n",
        "}\n",
        "static KuResult_str ku_pg_value(KuPgResult* r, int64_t row, int64_t col) {\n",
        "  if (!ku_pg_cell_in_bounds(r, row, col)) return (KuResult_str){ false, (KuString){0}, ku_pg_value_error() };\n",
        "  KuPgCell cell = r->cells[(size_t)row * r->cols + (size_t)col];\n",
        "  if (cell.len == KU_PG_NULL_CELL) return (KuResult_str){ false, (KuString){0}, ku_pg_static_error(\"null_value\", sizeof(\"null_value\") - 1, \"PostgreSQL NULL must be checked with is_null()\", sizeof(\"PostgreSQL NULL must be checked with is_null()\") - 1) };\n",
        "  if (cell.len == 0) return (KuResult_str){ true, (KuString){0}, (KuError){0} };\n",
        "  uint8_t* copied = (uint8_t*)malloc((size_t)cell.len);\n",
        "  if (!copied) return (KuResult_str){ false, (KuString){0}, ku_pg_out_of_memory_error() };\n",
        "  memcpy(copied, r->bytes + cell.offset, (size_t)cell.len);\n",
        "  return (KuResult_str){ true, (KuString){ copied, (size_t)cell.len, (size_t)cell.len, KU_STRING_OWNED }, (KuError){0} };\n",
        "}\n",
        "static KuResult_bool ku_pg_is_null(KuPgResult* r, int64_t row, int64_t col) {\n",
        "  if (!ku_pg_cell_in_bounds(r, row, col)) return (KuResult_bool){ false, false, ku_pg_value_error() };\n",
        "  return (KuResult_bool){ true, r->cells[(size_t)row * r->cols + (size_t)col].len == KU_PG_NULL_CELL, (KuError){0} };\n",
        "}\n",
    ));
    // Pooled client — bounded and thread-safe using a small Windows/POSIX sync
    // abstraction. The client owns every connection; queries borrow one and always
    // return it. In Ku's move-only ownership path, closing marks the client first and
    // defers destruction until registered borrowers and condition waiters leave.
    // These translation-unit-private raw helpers require one unique owner: callers
    // must not repeat close or start an operation concurrently with consuming close.
    // The raw pointer contract intentionally has no entrant refcount and does not
    // make a pointer reusable after close returns.
    if program_uses_pg_client(program) {
        out.push_str(concat!(
            "#if defined(_WIN32)\n",
            "typedef CRITICAL_SECTION KuPgMutex;\n",
            "typedef CONDITION_VARIABLE KuPgCond;\n",
            "static int ku_pg_sync_init(KuPgMutex* mutex, KuPgCond* cond) { InitializeCriticalSection(mutex); InitializeConditionVariable(cond); return 0; }\n",
            "static void ku_pg_sync_destroy(KuPgMutex* mutex, KuPgCond* cond) { (void)cond; DeleteCriticalSection(mutex); }\n",
            "static void ku_pg_mutex_lock(KuPgMutex* mutex) { EnterCriticalSection(mutex); }\n",
            "static void ku_pg_mutex_unlock(KuPgMutex* mutex) { LeaveCriticalSection(mutex); }\n",
            "static void ku_pg_cond_signal(KuPgCond* cond) { WakeConditionVariable(cond); }\n",
            "static void ku_pg_cond_broadcast(KuPgCond* cond) { WakeAllConditionVariable(cond); }\n",
            "static int ku_pg_cond_wait_ms(KuPgCond* cond, KuPgMutex* mutex, unsigned long long timeout_ms) {\n",
            "  DWORD timeout = timeout_ms > (unsigned long long)UINT32_MAX ? (DWORD)UINT32_MAX : (DWORD)timeout_ms;\n",
            "  if (SleepConditionVariableCS(cond, mutex, timeout)) return 0;\n",
            "  return GetLastError() == ERROR_TIMEOUT ? 1 : -1;\n",
            "}\n",
            "#else\n",
            "typedef pthread_mutex_t KuPgMutex;\n",
            "typedef pthread_cond_t KuPgCond;\n",
            "static int ku_pg_sync_init(KuPgMutex* mutex, KuPgCond* cond) {\n",
            "  int rc = pthread_mutex_init(mutex, 0);\n",
            "  if (rc != 0) return rc;\n",
            "#if defined(__APPLE__)\n",
            "  rc = pthread_cond_init(cond, 0);\n",
            "#else\n",
            "  pthread_condattr_t attr;\n",
            "  rc = pthread_condattr_init(&attr);\n",
            "  if (rc == 0) {\n",
            "    rc = pthread_condattr_setclock(&attr, CLOCK_MONOTONIC);\n",
            "    if (rc == 0) rc = pthread_cond_init(cond, &attr);\n",
            "    pthread_condattr_destroy(&attr);\n",
            "  }\n",
            "#endif\n",
            "  if (rc != 0) pthread_mutex_destroy(mutex);\n",
            "  return rc;\n",
            "}\n",
            "static void ku_pg_sync_destroy(KuPgMutex* mutex, KuPgCond* cond) {\n",
            "  if (pthread_cond_destroy(cond) != 0) { fputs(\"pg client condition destroy failed\\n\", stderr); exit(1); }\n",
            "  if (pthread_mutex_destroy(mutex) != 0) { fputs(\"pg client mutex destroy failed\\n\", stderr); exit(1); }\n",
            "}\n",
            "static void ku_pg_mutex_lock(KuPgMutex* mutex) { if (pthread_mutex_lock(mutex) != 0) { fprintf(stderr, \"pg client mutex lock failed\\n\"); exit(1); } }\n",
            "static void ku_pg_mutex_unlock(KuPgMutex* mutex) { if (pthread_mutex_unlock(mutex) != 0) { fprintf(stderr, \"pg client mutex unlock failed\\n\"); exit(1); } }\n",
            "static void ku_pg_cond_signal(KuPgCond* cond) { if (pthread_cond_signal(cond) != 0) { fprintf(stderr, \"pg client condition signal failed\\n\"); exit(1); } }\n",
            "static void ku_pg_cond_broadcast(KuPgCond* cond) { if (pthread_cond_broadcast(cond) != 0) { fprintf(stderr, \"pg client condition broadcast failed\\n\"); exit(1); } }\n",
            "static int ku_pg_cond_wait_ms(KuPgCond* cond, KuPgMutex* mutex, unsigned long long timeout_ms) {\n",
            "#if defined(__APPLE__)\n",
            "  struct timespec relative = { (time_t)(timeout_ms / 1000ULL), (long)((timeout_ms % 1000ULL) * 1000000ULL) };\n",
            "  int rc = pthread_cond_timedwait_relative_np(cond, mutex, &relative);\n",
            "#else\n",
            "  struct timespec deadline = {0};\n",
            "  if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) return -1;\n",
            "  deadline.tv_sec += (time_t)(timeout_ms / 1000ULL);\n",
            "  long extra_ns = (long)((timeout_ms % 1000ULL) * 1000000ULL);\n",
            "  if (deadline.tv_nsec > 999999999L - extra_ns) { deadline.tv_sec++; deadline.tv_nsec -= 1000000000L - extra_ns; } else deadline.tv_nsec += extra_ns;\n",
            "  int rc = pthread_cond_timedwait(cond, mutex, &deadline);\n",
            "#endif\n",
            "  return rc == 0 ? 0 : (rc == ETIMEDOUT ? 1 : -1);\n",
            "}\n",
            "#endif\n",
            "static unsigned long long ku_pg_now_ms(void) { return KU_PG_MONOTONIC_MS(); }\n",
            "#define KU_PG_CLIENT_DEFAULT_MAX_CONNECTIONS 8LL\n",
            "#define KU_PG_CLIENT_DEFAULT_MAX_WAITERS 64LL\n",
            "#define KU_PG_CLIENT_DEFAULT_CONNECT_TIMEOUT_MS 5000LL\n",
            "#define KU_PG_CLIENT_DEFAULT_ACQUIRE_TIMEOUT_MS 5000LL\n",
            "#define KU_PG_CLIENT_DEFAULT_QUERY_TIMEOUT_MS 30000LL\n",
            "struct KuPgClient { PGconn** conns; char* in_use; size_t size; size_t max_waiters; char* conninfo; size_t conninfo_len; unsigned long long connect_timeout_ms, acquire_timeout_ms, query_timeout_ms; KuPgMutex lock; KuPgCond cv; size_t active; size_t waiters; uint32_t consecutive_connect_failures; unsigned long long reconnect_not_before_ms; int closing; int finalizing; int connect_in_flight; int backoff_timer_armed; };\n",
            "static void ku_pg_client_dispose(KuPgClient* p);\n",
            "static int ku_pg_client_take_dispose_locked(KuPgClient* p) { if (!p->closing || p->finalizing || p->active != 0 || p->waiters != 0) return 0; p->finalizing = 1; return 1; }\n",
            "static unsigned long long ku_pg_client_saturating_add_ms(unsigned long long now, unsigned long long delay) { return ~0ULL - now < delay ? ~0ULL : now + delay; }\n",
            "static unsigned long long ku_pg_client_backoff_delay_ms(KuPgClient* p, unsigned long long now) {\n",
            "  uint32_t failures = p->consecutive_connect_failures; unsigned int shift = failures > 6U ? 6U : (failures ? failures - 1U : 0U);\n",
            "  unsigned long long window = 25ULL << shift; if (window > 1000ULL) window = 1000ULL;\n",
            "  unsigned long long mixed = (unsigned long long)(uintptr_t)p ^ now ^ ((unsigned long long)failures * 0x9e3779b97f4a7c15ULL);\n",
            "  mixed ^= mixed >> 30; mixed *= 0xbf58476d1ce4e5b9ULL; mixed ^= mixed >> 27; mixed *= 0x94d049bb133111ebULL; mixed ^= mixed >> 31;\n",
            "  unsigned long long lower = (window + 1ULL) / 2ULL; return lower + mixed % (window - lower + 1ULL);\n",
            "}\n",
            "static void ku_pg_client_record_connect_failure_locked(KuPgClient* p, unsigned long long now) {\n",
            "  if (p->consecutive_connect_failures != UINT32_MAX) p->consecutive_connect_failures++;\n",
            "  p->reconnect_not_before_ms = ku_pg_client_saturating_add_ms(now, ku_pg_client_backoff_delay_ms(p, now));\n",
            "}\n",
            "static void ku_pg_client_record_connect_success_locked(KuPgClient* p) { p->consecutive_connect_failures = 0; p->reconnect_not_before_ms = 0; }\n",
            "static KuError ku_pg_invalid_config(const char* message, size_t len) { return ku_pg_client_error(\"invalid_config\", sizeof(\"invalid_config\") - 1, message, len); }\n",
            "static KuError ku_pg_post_execution_session_state_error(void) { return ku_pg_client_error(\"session_state_unsupported\", sizeof(\"session_state_unsupported\") - 1, \"PostgreSQL statement completed or may have completed; session state is unsupported and its payload was discarded; never retry automatically\", sizeof(\"PostgreSQL statement completed or may have completed; session state is unsupported and its payload was discarded; never retry automatically\") - 1); }\n",
            "static KuValue* ku_pg_client_config_get(KuObject* config, const char* key, size_t len) { return config ? ku_object_get(config, ku_string_static((const uint8_t*)key, len)) : 0; }\n",
            "static int ku_pg_client_config_int(KuObject* config, const char* key, size_t key_len, int64_t fallback, int64_t minimum, int64_t maximum, int64_t* out, KuError* error) {\n",
            "  KuValue* value = ku_pg_client_config_get(config, key, key_len);\n",
            "  if (!value) { *out = fallback; return 1; }\n",
            "  if (value->tag != KU_INT) { *error = ku_pg_invalid_config(\"PostgreSQL client integer config field has the wrong type\", sizeof(\"PostgreSQL client integer config field has the wrong type\") - 1); return 0; }\n",
            "  if (value->as.i < minimum || value->as.i > maximum) { *error = ku_pg_invalid_config(\"PostgreSQL client integer config field is outside its allowed range\", sizeof(\"PostgreSQL client integer config field is outside its allowed range\") - 1); return 0; }\n",
            "  *out = value->as.i; return 1;\n",
            "}\n",
            "static int ku_pg_client_config_key_allowed(KuString key) {\n",
            "  static const char* allowed[] = { \"conninfo\", \"max_connections\", \"max_waiters\", \"connect_timeout_ms\", \"acquire_timeout_ms\", \"query_timeout_ms\" };\n",
            "  static const size_t lengths[] = { 8, 15, 11, 18, 18, 16 };\n",
            "  for (size_t i = 0; i < sizeof(allowed) / sizeof(allowed[0]); i++) if (ku_string_equal(key, ku_string_static((const uint8_t*)allowed[i], lengths[i]))) return 1;\n",
            "  return 0;\n",
            "}\n",
            "static KuResult_pg_client ku_pg_client_open(KuString conninfo, int64_t size, int64_t max_waiters, int64_t connect_timeout_ms, int64_t acquire_timeout_ms, int64_t query_timeout_ms) {\n",
            "  if (size < 1 || size > 256 || max_waiters < 0 || max_waiters > 4096 || connect_timeout_ms < 1 || connect_timeout_ms > 300000 || acquire_timeout_ms < 1 || acquire_timeout_ms > 300000 || query_timeout_ms < 1 || query_timeout_ms > 300000) return (KuResult_pg_client){ false, 0, ku_pg_invalid_config(\"PostgreSQL client configuration is outside its allowed range\", sizeof(\"PostgreSQL client configuration is outside its allowed range\") - 1) };\n",
            "  if (conninfo.len == SIZE_MAX || conninfo.len > KU_PG_MAX_CONNINFO_BYTES || (conninfo.len && !conninfo.ptr) || ku_pg_string_has_nul(conninfo)) return (KuResult_pg_client){ false, 0, ku_pg_invalid_config(\"PostgreSQL conninfo is invalid, too large, or contains a NUL byte\", sizeof(\"PostgreSQL conninfo is invalid, too large, or contains a NUL byte\") - 1) };\n",
            "  KuPgClient* p = (KuPgClient*)malloc(sizeof(KuPgClient));\n",
            "  if (!p) return (KuResult_pg_client){ false, 0, ku_pg_out_of_memory_error() };\n",
            "  memset(p, 0, sizeof(*p)); p->size = (size_t)size; p->max_waiters = (size_t)max_waiters; p->conninfo_len = conninfo.len; p->connect_timeout_ms = (unsigned long long)connect_timeout_ms; p->acquire_timeout_ms = (unsigned long long)acquire_timeout_ms; p->query_timeout_ms = (unsigned long long)query_timeout_ms;\n",
            "  p->conns = (PGconn**)calloc(p->size, sizeof(PGconn*));\n",
            "  p->in_use = (char*)calloc(p->size, 1);\n",
            "  p->conninfo = (char*)malloc(conninfo.len + 1);\n",
            "  if (!p->conns || !p->in_use || !p->conninfo) { free(p->conns); free(p->in_use); free(p->conninfo); free(p); return (KuResult_pg_client){ false, 0, ku_pg_out_of_memory_error() }; }\n",
            "  if (conninfo.len) memcpy(p->conninfo, conninfo.ptr, conninfo.len); p->conninfo[conninfo.len] = '\\0';\n",
            "  if (ku_pg_sync_init(&p->lock, &p->cv) != 0) { free(p->conns); free(p->in_use); ku_pg_wipe_secret(p->conninfo, p->conninfo_len); free(p->conninfo); free(p); return (KuResult_pg_client){ false, 0, ku_pg_client_error(\"sync_error\", sizeof(\"sync_error\") - 1, \"PostgreSQL client synchronization initialization failed\", sizeof(\"PostgreSQL client synchronization initialization failed\") - 1) }; }\n",
            "  unsigned long long initial_deadline = ku_pg_deadline_after_ms(p->connect_timeout_ms);\n",
            "  KuPgConnectAttempt initial_attempt = ku_pg_connect_until(p->conninfo, initial_deadline);\n",
            "  PGconn* initial = initial_attempt.conn;\n",
            "  if (!initial) {\n",
            "    ku_pg_wipe_secret(p->conninfo, p->conninfo_len); ku_pg_sync_destroy(&p->lock, &p->cv); free(p->conns); free(p->in_use); free(p->conninfo); free(p);\n",
            "    KuError initial_error = initial_attempt.outcome == KU_PG_CONNECT_OUT_OF_MEMORY ? ku_pg_out_of_memory_error() : (initial_attempt.outcome == KU_PG_CONNECT_TIMED_OUT ? ku_pg_static_error(\"connect_timeout\", sizeof(\"connect_timeout\") - 1, \"PostgreSQL client connection timed out\", sizeof(\"PostgreSQL client connection timed out\") - 1) : ku_pg_connect_failure_error());\n",
            "    return (KuResult_pg_client){ false, 0, initial_error };\n",
            "  }\n",
            "  p->conns[0] = initial;\n",
            "  return (KuResult_pg_client){ true, p, (KuError){0} };\n",
            "}\n",
            "static KuResult_pg_client ku_pg_client(KuObject* config) {\n",
            "  if (!config) return (KuResult_pg_client){ false, 0, ku_pg_invalid_config(\"pg.client requires a config object\", sizeof(\"pg.client requires a config object\") - 1) };\n",
            "  for (size_t i = 0; i < config->cap; i++) if (config->entries[i].used && !ku_pg_client_config_key_allowed(config->entries[i].key)) return (KuResult_pg_client){ false, 0, ku_pg_invalid_config(\"PostgreSQL client config contains an unknown field\", sizeof(\"PostgreSQL client config contains an unknown field\") - 1) };\n",
            "  KuValue* conninfo_value = ku_pg_client_config_get(config, \"conninfo\", sizeof(\"conninfo\") - 1);\n",
            "  if (!conninfo_value || conninfo_value->tag != KU_STR) return (KuResult_pg_client){ false, 0, ku_pg_invalid_config(\"pg.client config requires string field 'conninfo'\", sizeof(\"pg.client config requires string field 'conninfo'\") - 1) };\n",
            "  int64_t size, max_waiters, connect_timeout_ms, acquire_timeout_ms, query_timeout_ms; KuError error = (KuError){0};\n",
            "  if (!ku_pg_client_config_int(config, \"max_connections\", sizeof(\"max_connections\") - 1, KU_PG_CLIENT_DEFAULT_MAX_CONNECTIONS, 1, 256, &size, &error) || !ku_pg_client_config_int(config, \"max_waiters\", sizeof(\"max_waiters\") - 1, KU_PG_CLIENT_DEFAULT_MAX_WAITERS, 0, 4096, &max_waiters, &error) || !ku_pg_client_config_int(config, \"connect_timeout_ms\", sizeof(\"connect_timeout_ms\") - 1, KU_PG_CLIENT_DEFAULT_CONNECT_TIMEOUT_MS, 1, 300000, &connect_timeout_ms, &error) || !ku_pg_client_config_int(config, \"acquire_timeout_ms\", sizeof(\"acquire_timeout_ms\") - 1, KU_PG_CLIENT_DEFAULT_ACQUIRE_TIMEOUT_MS, 1, 300000, &acquire_timeout_ms, &error) || !ku_pg_client_config_int(config, \"query_timeout_ms\", sizeof(\"query_timeout_ms\") - 1, KU_PG_CLIENT_DEFAULT_QUERY_TIMEOUT_MS, 1, 300000, &query_timeout_ms, &error)) return (KuResult_pg_client){ false, 0, error };\n",
            "  return ku_pg_client_open(conninfo_value->as.s, size, max_waiters, connect_timeout_ms, acquire_timeout_ms, query_timeout_ms);\n",
            "}\n",
            "static void ku_pg_client_handoff_available_locked(KuPgClient* p) {\n",
            "  if (!p || p->waiters == 0) return;\n",
            "  for (size_t i = 0; i < p->size; i++) if (p->conns[i] && !p->in_use[i]) { ku_pg_cond_signal(&p->cv); return; }\n",
            "  if (p->connect_in_flight || ku_pg_now_ms() < p->reconnect_not_before_ms) return;\n",
            "  for (size_t i = 0; i < p->size; i++) if (!p->conns[i] && !p->in_use[i]) { ku_pg_cond_signal(&p->cv); return; }\n",
            "}\n",
            "static void ku_pg_client_release_backoff_timer_locked(KuPgClient* p, int owned) {\n",
            "  if (!owned) return; p->backoff_timer_armed = 0; if (p->waiters != 0) ku_pg_cond_signal(&p->cv);\n",
            "}\n",
            // Acquire a slot (blocking). Returns slot index and sets *out; on connect
            // failure returns -1 with a static structured error. A reserved slot has
            // conns[i]==NULL && in_use[i]==1, so no two threads pick the same slot.
            "static int ku_pg_client_acquire(KuPgClient* p, PGconn** out, KuError* err, unsigned long long operation_deadline) {\n",
            "  if (!p) { *err = ku_pg_client_error(\"client_closed\", sizeof(\"client_closed\") - 1, \"PostgreSQL client is closed\", sizeof(\"PostgreSQL client is closed\") - 1); return -1; }\n",
            "  unsigned long long started = ku_pg_now_ms();\n",
            "  unsigned long long deadline = ~0ULL - started < p->acquire_timeout_ms ? ~0ULL : started + p->acquire_timeout_ms;\n",
            "  if (__ku_handler_deadline != 0 && __ku_handler_deadline < deadline) deadline = __ku_handler_deadline;\n",
            "  if (operation_deadline != 0 && operation_deadline < deadline) deadline = operation_deadline;\n",
            "  int has_waited = 0;\n",
            "  ku_pg_mutex_lock(&p->lock);\n",
            "  for (;;) {\n",
            "    if (p->closing) { ku_pg_mutex_unlock(&p->lock); *err = ku_pg_client_error(\"client_closed\", sizeof(\"client_closed\") - 1, \"PostgreSQL client is closed\", sizeof(\"PostgreSQL client is closed\") - 1); return -1; }\n",
            "    unsigned long long now = ku_pg_now_ms();\n",
            "    if (now >= deadline) { ku_pg_client_handoff_available_locked(p); ku_pg_mutex_unlock(&p->lock); *err = ku_pg_client_error(\"acquire_timeout\", sizeof(\"acquire_timeout\") - 1, \"timed out waiting for a PostgreSQL client connection\", sizeof(\"timed out waiting for a PostgreSQL client connection\") - 1); return -1; }\n",
            "    int can_claim = has_waited || p->waiters == 0;\n",
            "    if (can_claim) for (size_t i = 0; i < p->size; i++) if (p->conns[i] && !p->in_use[i]) { p->in_use[i] = 1; p->active++; *out = p->conns[i]; ku_pg_mutex_unlock(&p->lock); return (int)i; }\n",
            "    int made = -1;\n",
            "    if (can_claim && !p->connect_in_flight && now >= p->reconnect_not_before_ms) for (size_t i = 0; i < p->size; i++) if (!p->conns[i] && !p->in_use[i]) { p->in_use[i] = 1; p->active++; p->connect_in_flight = 1; made = (int)i; break; }\n",
            "    if (made >= 0) {\n",
            "      now = ku_pg_now_ms();\n",
            "      if (now >= deadline) {\n",
            "        p->connect_in_flight = 0; p->in_use[made] = 0; p->active--; ku_pg_cond_signal(&p->cv); ku_pg_mutex_unlock(&p->lock);\n",
            "        *err = ku_pg_client_error(\"acquire_timeout\", sizeof(\"acquire_timeout\") - 1, \"timed out waiting for a PostgreSQL client connection\", sizeof(\"timed out waiting for a PostgreSQL client connection\") - 1); return -1;\n",
            "      }\n",
            "      ku_pg_mutex_unlock(&p->lock);\n",
            "      unsigned long long connect_budget_deadline = ku_pg_client_saturating_add_ms(now, p->connect_timeout_ms); int acquire_limited_connect = deadline <= connect_budget_deadline; unsigned long long connect_deadline = acquire_limited_connect ? deadline : connect_budget_deadline;\n",
            "      KuPgConnectAttempt connect_attempt = ku_pg_connect_until(p->conninfo, connect_deadline);\n",
            "      PGconn* h = connect_attempt.conn;\n",
            "      if (!h) {\n",
            "        KuError e = connect_attempt.outcome == KU_PG_CONNECT_OUT_OF_MEMORY ? ku_pg_out_of_memory_error() : (connect_attempt.outcome == KU_PG_CONNECT_TIMED_OUT ? (acquire_limited_connect ? ku_pg_client_error(\"acquire_timeout\", sizeof(\"acquire_timeout\") - 1, \"timed out acquiring a PostgreSQL client connection\", sizeof(\"timed out acquiring a PostgreSQL client connection\") - 1) : ku_pg_static_error(\"connect_timeout\", sizeof(\"connect_timeout\") - 1, \"timed out connecting a PostgreSQL client connection\", sizeof(\"timed out connecting a PostgreSQL client connection\") - 1)) : ku_pg_connect_failure_error());\n",
            "        ku_pg_mutex_lock(&p->lock); p->connect_in_flight = 0; ku_pg_client_record_connect_failure_locked(p, ku_pg_now_ms()); p->in_use[made] = 0; p->active--; int dispose = ku_pg_client_take_dispose_locked(p); if (p->closing) ku_pg_cond_broadcast(&p->cv); else ku_pg_cond_signal(&p->cv); ku_pg_mutex_unlock(&p->lock);\n",
            "        *err = e; if (dispose) ku_pg_client_dispose(p); return -1;\n",
            "      }\n",
            "      ku_pg_mutex_lock(&p->lock);\n",
            "      p->connect_in_flight = 0; ku_pg_client_record_connect_success_locked(p);\n",
            /* This thread can be descheduled after the poll helper succeeds and
               before it reacquires the pool lock. Re-check the absolute deadline
               while holding the lock so an expired reservation is rolled back
               instead of being installed and used by the query pump. */
            "      int connect_expired = ku_pg_now_ms() >= deadline;\n",
            "      if (p->closing || connect_expired) { int client_closing = p->closing; p->in_use[made] = 0; p->active--; int dispose = ku_pg_client_take_dispose_locked(p); if (p->closing) ku_pg_cond_broadcast(&p->cv); else ku_pg_cond_signal(&p->cv); ku_pg_mutex_unlock(&p->lock); PQfinish(h); *err = client_closing ? ku_pg_client_error(\"client_closed\", sizeof(\"client_closed\") - 1, \"PostgreSQL client is closed\", sizeof(\"PostgreSQL client is closed\") - 1) : ku_pg_client_error(\"acquire_timeout\", sizeof(\"acquire_timeout\") - 1, \"timed out acquiring a PostgreSQL client connection\", sizeof(\"timed out acquiring a PostgreSQL client connection\") - 1); if (dispose) ku_pg_client_dispose(p); return -1; }\n",
            "      p->conns[made] = h; ku_pg_cond_signal(&p->cv); ku_pg_mutex_unlock(&p->lock);\n",
            "      *out = h; return made;\n",
            "    }\n",
            "    now = ku_pg_now_ms();\n",
            "    if (now >= deadline) { ku_pg_client_handoff_available_locked(p); ku_pg_mutex_unlock(&p->lock); *err = ku_pg_client_error(\"acquire_timeout\", sizeof(\"acquire_timeout\") - 1, \"timed out waiting for a PostgreSQL client connection\", sizeof(\"timed out waiting for a PostgreSQL client connection\") - 1); return -1; }\n",
            "    if (p->waiters >= p->max_waiters) { ku_pg_mutex_unlock(&p->lock); *err = ku_pg_client_error(\"pool_busy\", sizeof(\"pool_busy\") - 1, \"PostgreSQL client waiter limit reached\", sizeof(\"PostgreSQL client waiter limit reached\") - 1); return -1; }\n",
            "    unsigned long long wait_deadline = deadline; int owns_backoff_timer = 0;\n",
            "    if (p->reconnect_not_before_ms > now && !p->backoff_timer_armed) { p->backoff_timer_armed = 1; owns_backoff_timer = 1; if (p->reconnect_not_before_ms < wait_deadline) wait_deadline = p->reconnect_not_before_ms; }\n",
            "    unsigned long long remaining = wait_deadline - now;\n",
            "    p->waiters++; int wait_result = ku_pg_cond_wait_ms(&p->cv, &p->lock, remaining); p->waiters--; ku_pg_client_release_backoff_timer_locked(p, owns_backoff_timer);\n",
            "    if (p->closing) { int dispose = ku_pg_client_take_dispose_locked(p); ku_pg_cond_broadcast(&p->cv); ku_pg_mutex_unlock(&p->lock); *err = ku_pg_client_error(\"client_closed\", sizeof(\"client_closed\") - 1, \"PostgreSQL client is closed\", sizeof(\"PostgreSQL client is closed\") - 1); if (dispose) ku_pg_client_dispose(p); return -1; }\n",
            "    if (wait_result != 0) { now = ku_pg_now_ms(); if (wait_result == 1 && wait_deadline < deadline && now < deadline) { has_waited = 1; continue; } ku_pg_client_handoff_available_locked(p); ku_pg_mutex_unlock(&p->lock); *err = wait_result == 1 ? ku_pg_client_error(\"acquire_timeout\", sizeof(\"acquire_timeout\") - 1, \"timed out waiting for a PostgreSQL client connection\", sizeof(\"timed out waiting for a PostgreSQL client connection\") - 1) : ku_pg_client_error(\"sync_error\", sizeof(\"sync_error\") - 1, \"failed waiting for a PostgreSQL client connection\", sizeof(\"failed waiting for a PostgreSQL client connection\") - 1); return -1; }\n",
            "    has_waited = 1;\n",
            "  }\n",
            "}\n",
            // A single-query pool API must never hand a transaction/session in an
            // unfinished transactional state to the next borrower. `PQstatus`
            // alone does not detect INTRANS/INERROR/COPY protocol states.
            "static int ku_pg_client_cleanup_connection(PGconn* c, int broken, unsigned long long deadline) {\n",
            "  if (!c || broken || PQstatus(c) != KU_PG_CONNECTION_OK || ku_pg_deadline_expired(deadline)) return 1;\n",
            "  int tx = PQtransactionStatus(c);\n",
            /* Never issue a hidden synchronous ROLLBACK here: it has no query
               deadline and could pin a worker indefinitely. A non-idle session is
               discarded and lazily replaced on the next acquisition. */
            "  if (tx != KU_PQTRANS_IDLE) return 1;\n",
            /* The default shared client must not carry ROLE, search_path,
               LISTEN, advisory locks, temp tables, or other session state to
               the next borrower. Reuse the nonblocking query pump and the
               original operation budget; reset failure only evicts this slot
               because the user's detached result is already complete. */
            "  int reset_broken = 0;\n",
            "  KuResult_pg_result reset = ku_pg_query_params_validated_impl(c, ku_string_static((const uint8_t*)\"DISCARD ALL\", sizeof(\"DISCARD ALL\") - 1), (KuArray_str){0}, 0, deadline, &reset_broken);\n",
            "  int reset_failed = reset_broken || !reset.ok;\n",
            "  if (reset.ok) ku_drop_pg_result(&reset.value); else ku_error_drop(&reset.error);\n",
            "  if (reset_failed || ku_pg_deadline_expired(deadline) || PQstatus(c) != KU_PG_CONNECTION_OK || PQtransactionStatus(c) != KU_PQTRANS_IDLE) return 1;\n",
            "  return !ku_pg_connection_is_utf8(c);\n",
            "}\n",
            "static void ku_pg_client_release(KuPgClient* p, int slot, int broken, unsigned long long deadline) {\n",
            "  ku_pg_mutex_lock(&p->lock);\n",
            "  int cleanup_allowed = !p->closing; PGconn* connection = p->conns[slot];\n",
            "  ku_pg_mutex_unlock(&p->lock);\n",
            "  if (!cleanup_allowed) broken = 1; else broken = ku_pg_client_cleanup_connection(connection, broken, deadline);\n",
            "  ku_pg_mutex_lock(&p->lock);\n",
            "  PGconn* discard = 0;\n",
            "  if ((broken || p->closing) && p->conns[slot]) { discard = p->conns[slot]; p->conns[slot] = 0; }\n",
            "  p->in_use[slot] = 0; if (p->active > 0) p->active--;\n",
            "  int dispose = ku_pg_client_take_dispose_locked(p);\n",
            "  if (p->closing) ku_pg_cond_broadcast(&p->cv); else ku_pg_cond_signal(&p->cv); ku_pg_mutex_unlock(&p->lock);\n",
            "  if (discard) PQfinish(discard);\n",
            "  if (dispose) ku_pg_client_dispose(p);\n",
            "}\n",
            "static KuResult_pg_result ku_pg_client_query(KuPgClient* p, KuString sql, KuArray_str params) {\n",
            "  if (!p) return (KuResult_pg_result){ false, 0, ku_pg_client_error(\"client_closed\", sizeof(\"client_closed\") - 1, \"PostgreSQL client is closed\", sizeof(\"PostgreSQL client is closed\") - 1) };\n",
            "  unsigned long long deadline = ku_pg_deadline_after_ms(p->query_timeout_ms);\n",
            "  KuError sql_error = (KuError){0}; if (!ku_pg_validate_sql_input(sql, &sql_error, deadline)) return (KuResult_pg_result){ false, 0, sql_error };\n",
            "  int session_control = ku_pg_sql_has_explicit_session_control(sql, deadline);\n",
            "  if (session_control == -2) return (KuResult_pg_result){ false, 0, ku_pg_query_timeout_error() };\n",
            "  if (session_control != 0) return (KuResult_pg_result){ false, 0, ku_pg_client_error(\"session_state_unsupported\", sizeof(\"session_state_unsupported\") - 1, \"PostgreSQL statement was not sent because explicit transaction or session-control SQL is unsupported by the pooled client\", sizeof(\"PostgreSQL statement was not sent because explicit transaction or session-control SQL is unsupported by the pooled client\") - 1) };\n",
            "  size_t param_bytes = 0; KuError param_error = (KuError){0};\n",
            "  if (!ku_pg_validate_query_params(params, &param_bytes, &param_error, deadline)) return (KuResult_pg_result){ false, 0, param_error };\n",
            "  PGconn* c = 0; KuError err = (KuError){0};\n",
            "  int slot = ku_pg_client_acquire(p, &c, &err, deadline);\n",
            "  if (slot < 0) return (KuResult_pg_result){ false, 0, err };\n",
            "  int broken = 0; KuResult_pg_result r = ku_pg_query_params_all_validated_impl(c, sql, params, param_bytes, deadline, &broken);\n",
            "  if (r.ok && PQtransactionStatus(c) != KU_PQTRANS_IDLE) { ku_drop_pg_result(&r.value); r = (KuResult_pg_result){ false, 0, ku_pg_post_execution_session_state_error() }; broken = 1; }\n",
            "  ku_pg_client_release(p, slot, broken || PQstatus(c) != KU_PG_CONNECTION_OK, deadline);\n",
            "  return r;\n",
            "}\n",
            "static void ku_pg_client_dispose(KuPgClient* p) {\n",
            "  if (!p) return;\n",
            "  for (size_t i = 0; i < p->size; i++) if (p->conns[i]) PQfinish(p->conns[i]);\n",
            "  ku_pg_wipe_secret(p->conninfo, p->conninfo_len); ku_pg_sync_destroy(&p->lock, &p->cv); free(p->conns); free(p->in_use); free(p->conninfo); free(p);\n",
            "}\n",
            "static void ku_pg_client_close_owned(KuPgClient* p) {\n",
            "  if (!p) return;\n",
            "  ku_pg_mutex_lock(&p->lock);\n",
            "  p->closing = 1; int dispose = ku_pg_client_take_dispose_locked(p);\n",
            "  ku_pg_cond_broadcast(&p->cv); ku_pg_mutex_unlock(&p->lock);\n",
            "  if (dispose) ku_pg_client_dispose(p);\n",
            "}\n",
            "static KuPgClient* ku_move_pg_client(KuPgClient** p) { KuPgClient* m = *p; *p = 0; return m; }\n",
            "static void ku_drop_pg_client(KuPgClient** p) { if (p && *p) { KuPgClient* owned = *p; *p = 0; ku_pg_client_close_owned(owned); } }\n",
            "static KuPgClient* ku_clone_pg_client(KuPgClient* c) { (void)c; fprintf(stderr, \"cannot clone a pg client\\n\"); exit(1); }\n",
            "static uint8_t ku_pg_client_close(KuPgClient* p) { ku_pg_client_close_owned(p); return 0; }\n",
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
        IrType::Closure { params, ret, .. } => {
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
        IrInst::BindOk { ty, result, .. } => ir_type_uses_http(ty) || ir_type_uses_http(&result.ty),
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
fn emit_http_types(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    if !program_uses_http(program) {
        return Ok(());
    }
    // Function prototypes and closure signatures may return/borrow the opaque
    // server pointer before the socket runtime completes its private layout.
    out.push_str("typedef struct KuHttpServer KuHttpServer;\n");
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

/// The admission-controlled platform-socket HTTP server runtime. Emitted after the
/// Result ABI (it calls handlers whose return is `KuResult_struct___ku_http_response`)
/// and after `KuEnvHeader` (route env release). Uppercase method + exact-path
/// (query-stripped, segment-normalized) routing, 404/405 fallbacks matching the
/// interpreter. `KU_HTTP_MAX_REQUESTS` (env) bounds the loop for leak/ASan runs.
fn emit_http_runtime(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    if !program_uses_http(program) {
        return Ok(());
    }
    out.push_str(
        r####"#define KU_NATIVE_RUNTIME_HTTP_SOCKET 1
#define KU_HTTP_MAX_SEGS 64
/* Longest accepted request target, pinned to the interpreter's
   MAX_REQUEST_TARGET_BYTES (src/stdlib/http.rs). A longer target is answered with
   414 before it is copied or routed -- never truncated: truncating would let two
   distinct paths that share a long prefix resolve to the same route. */
#define KU_HTTP_MAX_TARGET 8192
#define KU_HTTP_MAX_TIMEOUT_MS 300000LL
#define KU_HTTP_MAX_HEADER_BYTES 65536LL
#define KU_HTTP_MAX_BODY_BYTES 16777216LL
#define KU_HTTP_MAX_CONNECTIONS 4096LL
#define KU_HTTP_MAX_ACTIVE_REQUESTS 1024LL
#define KU_HTTP_MAX_PENDING_REQUESTS 8192LL
#define KU_HTTP_READ_CHUNK 8192

/* Private platform boundary. HTTP parsing/routing above the socket calls is
   byte-for-byte shared by Windows, Linux and macOS. */
#if defined(_WIN32)
typedef SOCKET KuHttpSocket;
#define KU_HTTP_INVALID_SOCKET INVALID_SOCKET
typedef volatile LONG KuHttpAtomicCounter;
static void ku_http_atomic_store(KuHttpAtomicCounter* counter, long value) { InterlockedExchange(counter, value); }
static long ku_http_atomic_increment(KuHttpAtomicCounter* counter) { return (long)InterlockedIncrement(counter); }
static void ku_http_atomic_decrement(KuHttpAtomicCounter* counter) { (void)InterlockedDecrement(counter); }
#else
typedef int KuHttpSocket;
#define KU_HTTP_INVALID_SOCKET (-1)
typedef _Atomic long KuHttpAtomicCounter;
static void ku_http_atomic_store(KuHttpAtomicCounter* counter, long value) { atomic_store_explicit(counter, value, memory_order_relaxed); }
static long ku_http_atomic_increment(KuHttpAtomicCounter* counter) { return atomic_fetch_add_explicit(counter, 1, memory_order_acq_rel) + 1; }
static void ku_http_atomic_decrement(KuHttpAtomicCounter* counter) { (void)atomic_fetch_sub_explicit(counter, 1, memory_order_acq_rel); }
#endif

static unsigned long long ku_http_now_ms(void) { return __ku_handler_now_ms(); }

static int ku_http_socket_last_error(void) {
#if defined(_WIN32)
  return WSAGetLastError();
#else
  return errno;
#endif
}

static int ku_http_socket_error_interrupted(int error) {
#if defined(_WIN32)
  return error == WSAEINTR;
#else
  return error == EINTR;
#endif
}

/* accept(2) may surface already-aborted connections and pending network errors
   even though the listening socket itself is still healthy. A remote peer can
   trigger these, so they must never consume a stop-the-server retry budget. */
static int ku_http_socket_error_accept_peer_transient(int error) {
#if defined(_WIN32)
  return error == WSAEINTR || error == WSAECONNRESET || error == WSAECONNABORTED;
#else
  if (error == EINTR || error == ECONNABORTED) return 1;
#ifdef EPROTO
  if (error == EPROTO) return 1;
#endif
#ifdef ENETDOWN
  if (error == ENETDOWN) return 1;
#endif
#ifdef ENOPROTOOPT
  if (error == ENOPROTOOPT) return 1;
#endif
#ifdef EHOSTDOWN
  if (error == EHOSTDOWN) return 1;
#endif
#ifdef ENONET
  if (error == ENONET) return 1;
#endif
#ifdef EHOSTUNREACH
  if (error == EHOSTUNREACH) return 1;
#endif
#ifdef EOPNOTSUPP
  if (error == EOPNOTSUPP) return 1;
#endif
#ifdef ENETUNREACH
  if (error == ENETUNREACH) return 1;
#endif
  return 0;
#endif
}

/* Local descriptor/memory pressure is different: retry it with a longer bounded
   backoff, then fail rather than leave a resident process spinning forever. */
static int ku_http_socket_error_accept_resource_pressure(int error) {
#if defined(_WIN32)
  return error == WSAEMFILE || error == WSAENOBUFS;
#else
  return error == EMFILE || error == ENFILE || error == ENOBUFS || error == ENOMEM;
#endif
}

static int ku_http_net_init(void) {
#if defined(_WIN32)
  WSADATA data;
  return WSAStartup(MAKEWORD(2, 2), &data) == 0 ? 0 : -1;
#else
  return 0;
#endif
}

static void ku_http_net_cleanup(void) {
#if defined(_WIN32)
  WSACleanup();
#endif
}

static void ku_http_socket_close(KuHttpSocket socket_value) {
  if (socket_value == KU_HTTP_INVALID_SOCKET) return;
#if defined(_WIN32)
  closesocket(socket_value);
#else
  /* close(2) must not be retried after EINTR; the descriptor may already have
     been released and reused by another thread. */
  (void)close(socket_value);
#endif
}

static void ku_http_socket_shutdown_write(KuHttpSocket socket_value) {
#if defined(_WIN32)
  (void)shutdown(socket_value, SD_SEND);
#else
  (void)shutdown(socket_value, SHUT_WR);
#endif
}

static int ku_http_socket_suppress_sigpipe(KuHttpSocket socket_value) {
#if defined(__APPLE__)
  int enabled = 1;
  return setsockopt(socket_value, SOL_SOCKET, SO_NOSIGPIPE, &enabled, sizeof(enabled)) == 0 ? 0 : -1;
#else
  (void)socket_value;
  return 0;
#endif
}

static int ku_http_socket_set_send_timeout(KuHttpSocket socket_value, uint32_t timeout_ms) {
#if defined(_WIN32)
  DWORD timeout = (DWORD)timeout_ms;
  return setsockopt(socket_value, SOL_SOCKET, SO_SNDTIMEO, (const char*)&timeout, sizeof(timeout)) == 0 ? 0 : -1;
#else
  struct timeval timeout;
  timeout.tv_sec = (time_t)(timeout_ms / 1000U);
  timeout.tv_usec = (suseconds_t)((timeout_ms % 1000U) * 1000U);
  return setsockopt(socket_value, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) == 0 ? 0 : -1;
#endif
}

static int ku_http_socket_send(KuHttpSocket socket_value, const char* data, size_t len) {
  size_t chunk = len > (size_t)INT_MAX ? (size_t)INT_MAX : len;
#if defined(_WIN32)
  return send(socket_value, data, (int)chunk, 0);
#elif defined(__APPLE__)
  ssize_t sent = send(socket_value, data, chunk, 0);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#elif defined(MSG_NOSIGNAL)
  ssize_t sent = send(socket_value, data, chunk, MSG_NOSIGNAL);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#else
#error "std.http POSIX transport requires MSG_NOSIGNAL or SO_NOSIGPIPE"
#endif
}

static int ku_http_socket_recv(KuHttpSocket socket_value, char* data, size_t len) {
  size_t chunk = len > (size_t)INT_MAX ? (size_t)INT_MAX : len;
#if defined(_WIN32)
  return recv(socket_value, data, (int)chunk, 0);
#else
  ssize_t received = recv(socket_value, data, chunk, 0);
  return received > (ssize_t)INT_MAX ? INT_MAX : (int)received;
#endif
}

/* 1 = readable/hangup to be diagnosed by recv, 0 = deadline, -1 = error.
   POSIX uses poll rather than select so large numeric file descriptors never
   index outside fd_set. EINTR retries are bounded by the absolute deadline. */
static int ku_http_socket_wait_readable(KuHttpSocket socket_value, unsigned long long deadline) {
  for (;;) {
    unsigned long long now = ku_http_now_ms();
    if (now >= deadline) return 0;
    unsigned long long remaining = deadline - now;
#if defined(_WIN32)
    struct timeval wait;
    wait.tv_sec = (long)(remaining / 1000ULL);
    wait.tv_usec = (long)((remaining % 1000ULL) * 1000ULL);
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(socket_value, &readable);
    int selected = select(0, &readable, NULL, NULL, &wait);
#else
    int wait_ms = remaining > (unsigned long long)INT_MAX ? INT_MAX : (int)remaining;
    if (wait_ms == 0) wait_ms = 1;
    struct pollfd descriptor;
    descriptor.fd = socket_value;
    descriptor.events = POLLIN;
    descriptor.revents = 0;
    int selected = poll(&descriptor, 1, wait_ms);
    if (selected > 0 && (descriptor.revents & POLLNVAL)) return -1;
#endif
    if (selected > 0) return 1;
    if (selected == 0) return 0;
    if (!ku_http_socket_error_interrupted(ku_http_socket_last_error())) return -1;
  }
}

/* Apply a caller-bounded accept retry delay. Windows Sleep cannot be interrupted.
   POSIX poll-with-no-fds is retried only until the same absolute deadline, so a
   signal storm cannot turn the delay into a busy loop. */
static void ku_http_accept_retry_delay(uint32_t delay_ms) {
#if defined(_WIN32)
  Sleep((DWORD)delay_ms);
#else
  unsigned long long start = ku_http_now_ms();
  unsigned long long deadline = (~0ULL - start < (unsigned long long)delay_ms)
    ? ~0ULL
    : start + (unsigned long long)delay_ms;
  for (;;) {
    unsigned long long now = ku_http_now_ms();
    if (now >= deadline) break;
    unsigned long long remaining = deadline - now;
    int wait_ms = remaining > (unsigned long long)INT_MAX ? INT_MAX : (int)remaining;
    if (wait_ms == 0) wait_ms = 1;
    int wait_result = poll(NULL, 0, wait_ms);
    if (wait_result >= 0 || errno != EINTR) break;
  }
#endif
}
/* A terminal handler registered at a trie node, one per HTTP method. Parameter
   names belong to the route/handler, not to shared trie nodes: two methods (or
   two routes with a shared param-shaped prefix) may legitimately use different
   names for the same captured segment positions. */
typedef struct { char* method; char** param_names; size_t nparams; void* invoke; void* env; int arity; int returns_result; } KuHttpHandler;
/* Stage 8b: a routing trie node. Each node holds static children (keyed by the
   literal path segment) and at most one `{param}` child. Matching prefers a
   static child over the param child at every segment (with backtracking), which
   mirrors the interpreter's exact-shape-before-param-scan lookup. */
typedef struct KuHttpNode {
  char* seg;                     /* static segment label for this node (NULL at root) */
  struct KuHttpNode** children;  /* static children */
  size_t nchild; size_t cchild;
  struct KuHttpNode* param;      /* single `{param}` child, or NULL */
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
struct KuHttpServer {
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
};
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
    case KU_FLOAT: return "float";
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
static long long ku_http_cfg_int(KuObject* config, const char* key, long long dflt, long long maximum) {
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
  if ((long long)v->as.i > maximum) {
    fprintf(stderr, "http config field '%s' must be at most %lld\n", key, maximum);
    exit(1);
  }
  return (long long)v->as.i;
}
static void ku_http_validate_server(KuHttpServer* s) {
  struct KuHttpLimit { const char* name; long long value; long long maximum; } limits[] = {
    { "max_connections", s->max_connections, KU_HTTP_MAX_CONNECTIONS },
    { "max_active_requests", s->max_active_requests, KU_HTTP_MAX_ACTIVE_REQUESTS },
    { "max_pending_requests", s->max_pending_requests, KU_HTTP_MAX_PENDING_REQUESTS },
    { "handler_timeout_ms", s->handler_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "max_body_bytes", s->max_body_bytes, KU_HTTP_MAX_BODY_BYTES },
    { "max_header_bytes", s->max_header_bytes, KU_HTTP_MAX_HEADER_BYTES },
    { "read_header_timeout_ms", s->read_header_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "read_body_timeout_ms", s->read_body_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "write_timeout_ms", s->write_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "idle_timeout_ms", s->idle_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS }
  };
  for (size_t i = 0; i < sizeof(limits) / sizeof(limits[0]); i++) {
    if (limits[i].value <= 0) {
      fprintf(stderr, "http config field '%s' must be a positive int\n", limits[i].name); exit(1);
    }
    if (limits[i].value > limits[i].maximum) {
      fprintf(stderr, "http config field '%s' must be at most %lld\n", limits[i].name, limits[i].maximum); exit(1);
    }
  }
}
static int ku_http_server_config_key_allowed(KuString key) {
  static const char* allowed[] = {
    "read_header_timeout_ms", "read_body_timeout_ms", "write_timeout_ms",
    "idle_timeout_ms", "handler_timeout_ms", "max_body_bytes",
    "max_header_bytes", "max_connections", "max_active_requests",
    "max_pending_requests"
  };
  static const size_t lengths[] = { 22, 20, 16, 15, 18, 14, 16, 15, 19, 20 };
  for (size_t i = 0; i < sizeof(allowed) / sizeof(allowed[0]); i++) {
    if (ku_string_equal(key, ku_string_static((const uint8_t*)allowed[i], lengths[i]))) return 1;
  }
  return 0;
}
static void ku_http_validate_server_config_keys(KuObject* config) {
  if (!config) return;
  for (size_t i = 0; i < config->cap; i++) {
    KuEntry* entry = &config->entries[i];
    if (!entry->used || ku_http_server_config_key_allowed(entry->key)) continue;
    fprintf(stderr, "unknown http config field '");
    if (entry->key.len && entry->key.ptr) fwrite(entry->key.ptr, 1, entry->key.len, stderr);
    fprintf(stderr, "'\n");
    exit(1);
  }
}
/* `http.server(config)` / `http.service(config)`: start from the defaults, then
   override each admission-control limit present in the config object. */
static KuHttpServer* ku_http_server_new_cfg(KuObject* config) {
  ku_http_validate_server_config_keys(config);
  KuHttpServer* s = ku_http_server_new();
  s->max_connections = ku_http_cfg_int(config, "max_connections", s->max_connections, KU_HTTP_MAX_CONNECTIONS);
  s->max_active_requests = ku_http_cfg_int(config, "max_active_requests", s->max_active_requests, KU_HTTP_MAX_ACTIVE_REQUESTS);
  s->max_pending_requests = ku_http_cfg_int(config, "max_pending_requests", s->max_pending_requests, KU_HTTP_MAX_PENDING_REQUESTS);
  s->handler_timeout_ms = ku_http_cfg_int(config, "handler_timeout_ms", s->handler_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->max_body_bytes = ku_http_cfg_int(config, "max_body_bytes", s->max_body_bytes, KU_HTTP_MAX_BODY_BYTES);
  s->max_header_bytes = ku_http_cfg_int(config, "max_header_bytes", s->max_header_bytes, KU_HTTP_MAX_HEADER_BYTES);
  s->read_header_timeout_ms = ku_http_cfg_int(config, "read_header_timeout_ms", s->read_header_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->read_body_timeout_ms = ku_http_cfg_int(config, "read_body_timeout_ms", s->read_body_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->write_timeout_ms = ku_http_cfg_int(config, "write_timeout_ms", s->write_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->idle_timeout_ms = ku_http_cfg_int(config, "idle_timeout_ms", s->idle_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  ku_http_validate_server(s);
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
static KuHttpNode* ku_http_node_add_param(KuHttpNode* node) {
  if (!node->param) node->param = ku_http_node_new();
  return node->param;
}
static int ku_http_is_tchar(unsigned char c) {
  return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
         c == '!' || c == '#' || c == '$' || c == '%' || c == '&' || c == '\'' ||
         c == '*' || c == '+' || c == '-' || c == '.' || c == '^' || c == '_' ||
         c == '`' || c == '|' || c == '~';
}
static int ku_http_is_hex(unsigned char c) {
  return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'F') || (c >= 'a' && c <= 'f');
}
static int ku_http_is_uri_pchar(unsigned char c) {
  return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
         c == '-' || c == '.' || c == '_' || c == '~' || c == '!' || c == '$' ||
         c == '&' || c == '\'' || c == '(' || c == ')' || c == '*' || c == '+' ||
         c == ',' || c == ';' || c == '=' || c == ':' || c == '@';
}
static int ku_http_valid_pchar_sequence(const uint8_t* ptr, size_t len) {
  size_t i = 0;
  while (i < len) {
    if (ptr[i] == '%') {
      if (i + 2 >= len || !ku_http_is_hex(ptr[i + 1]) || !ku_http_is_hex(ptr[i + 2])) return 0;
      i += 3;
    } else if (ku_http_is_uri_pchar(ptr[i])) {
      i++;
    } else {
      return 0;
    }
  }
  return 1;
}
static int ku_http_utf8_valid(const uint8_t* data, size_t len) {
  if (len != 0 && !data) return 0;
  size_t i = 0;
  while (i < len) {
    uint8_t c = data[i];
    if (c <= 0x7f) { i++; continue; }
    if (c >= 0xc2 && c <= 0xdf) {
      if (i + 1 >= len || (data[i + 1] & 0xc0) != 0x80) return 0;
      i += 2; continue;
    }
    if (c == 0xe0) {
      if (i + 2 >= len || data[i + 1] < 0xa0 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if ((c >= 0xe1 && c <= 0xec) || (c >= 0xee && c <= 0xef)) {
      if (i + 2 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xed) {
      if (i + 2 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x9f || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xf0) {
      if (i + 3 >= len || data[i + 1] < 0x90 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c >= 0xf1 && c <= 0xf3) {
      if (i + 3 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c == 0xf4) {
      if (i + 3 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x8f || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    return 0;
  }
  return 1;
}
/* RFC 9112 origin-form with RFC 3986 pchar/query characters. This rejects
   whitespace, controls, backslashes, fragments and malformed percent escapes
   before any C-string operation or routing decision. */
static int ku_http_valid_origin_form(const uint8_t* ptr, size_t len) {
  if (!ptr || len == 0 || ptr[0] != '/') return 0;
  int in_query = 0;
  size_t i = 0;
  while (i < len) {
    uint8_t c = ptr[i];
    if (c == '?' && !in_query) { in_query = 1; i++; continue; }
    if (c == '/' || (in_query && c == '?')) { i++; continue; }
    if (c == '%') {
      if (i + 2 >= len || !ku_http_is_hex(ptr[i + 1]) || !ku_http_is_hex(ptr[i + 2])) return 0;
      i += 3;
      continue;
    }
    if (!ku_http_is_uri_pchar(c)) return 0;
    i++;
  }
  return 1;
}
static int ku_http_valid_route_param(const uint8_t* ptr, size_t len) {
  if (len == 0 || !((ptr[0] >= 'A' && ptr[0] <= 'Z') ||
                    (ptr[0] >= 'a' && ptr[0] <= 'z') || ptr[0] == '_')) return 0;
  for (size_t i = 1; i < len; i++) {
    uint8_t c = ptr[i];
    if (!((c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') ||
          (c >= 'a' && c <= 'z') || c == '_')) return 0;
  }
  return 1;
}
static int ku_http_valid_route_path(const uint8_t* ptr, size_t len, size_t* segment_count) {
  if (!ptr || len == 0 || ptr[0] != '/') return 0;
  size_t count = 0, i = 0;
  while (i < len) {
    while (i < len && ptr[i] == '/') i++;
    size_t start = i;
    while (i < len && ptr[i] != '/') i++;
    size_t seg_len = i - start;
    if (seg_len == 0) continue;
    count++;
    const uint8_t* seg = ptr + start;
    if (seg[0] == '{' || seg[seg_len - 1] == '}') {
      if (seg_len < 3 || seg[0] != '{' || seg[seg_len - 1] != '}' ||
          !ku_http_valid_route_param(seg + 1, seg_len - 2)) return 0;
    } else {
      for (size_t k = 0; k < seg_len; k++) if (seg[k] == ':') return 0;
      if (!ku_http_valid_pchar_sequence(seg, seg_len)) return 0;
    }
  }
  if (segment_count) *segment_count = count;
  return 1;
}
static KuHttpServer* ku_http_server_add_route(KuHttpServer* s, KuString method, KuString path, void* invoke, void* env, int arity, int returns_result) {
  if (method.len == 0 || method.len > 32) {
    fprintf(stderr, "http route method must be a valid HTTP token of at most 32 bytes\n"); exit(1);
  }
  for (size_t i = 0; i < method.len; i++) if (!ku_http_is_tchar(((const uint8_t*)method.ptr)[i])) {
    fprintf(stderr, "http route method must be a valid HTTP token of at most 32 bytes\n"); exit(1);
  }
  if (path.len > (size_t)KU_HTTP_MAX_TARGET) {
    fprintf(stderr, "http route path must be at most %d bytes\n", KU_HTTP_MAX_TARGET); exit(1);
  }
  size_t route_segments = 0;
  if (!ku_http_valid_route_path((const uint8_t*)path.ptr, path.len, &route_segments)) {
    fprintf(stderr, "invalid http route path\n"); exit(1);
  }
  if (route_segments > (size_t)KU_HTTP_MAX_SEGS) {
    fprintf(stderr, "http route path must contain at most %d segments\n", KU_HTTP_MAX_SEGS); exit(1);
  }
  /* Normalize into a buffer sized to the validated path, never a fixed one. */
  size_t pcap = path.len + 2;
  char* pbuf = (char*)malloc(pcap);
  if (!pbuf) { fprintf(stderr, "out of memory\n"); exit(1); }
  ku_http_normalize_path((const char*)path.ptr, path.len, pbuf, pcap);
  size_t plen = strlen(pbuf);
  KuHttpNode* node = s->root;
  const char* route_param_names[KU_HTTP_MAX_SEGS];
  size_t route_param_name_lens[KU_HTTP_MAX_SEGS];
  size_t route_param_count = 0;
  size_t i = 0;
  while (i < plen) {
    while (i < plen && pbuf[i] == '/') i++;
    size_t start = i;
    while (i < plen && pbuf[i] != '/') i++;
    size_t seg_len = i - start;
    if (seg_len == 0) continue;
    const char* seg = pbuf + start;
    if (seg_len >= 2 && seg[0] == '{' && seg[seg_len - 1] == '}') {
      size_t param_len = seg_len - 2;
      for (size_t p = 0; p < route_param_count; p++) {
        if (route_param_name_lens[p] == param_len &&
            memcmp(route_param_names[p], seg + 1, param_len) == 0) {
          fputs("duplicate http route param '", stderr);
          fwrite(seg + 1, 1, param_len, stderr);
          fputs("'\n", stderr);
          exit(1);
        }
      }
      route_param_names[route_param_count] = seg + 1;
      route_param_name_lens[route_param_count] = param_len;
      route_param_count++;
      node = ku_http_node_add_param(node);
    } else {
      node = ku_http_node_add_static(node, seg, seg_len);
    }
  }
  for (size_t i = 0; i < node->nh; i++) {
    size_t registered_len = strlen(node->handlers[i].method);
    if (registered_len == method.len &&
        memcmp(node->handlers[i].method, method.ptr, method.len) == 0) {
      fprintf(stderr, "duplicate http route\n"); exit(1);
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
  h->param_names = NULL;
  h->nparams = route_param_count;
  if (route_param_count > 0) {
    h->param_names = (char**)calloc(route_param_count, sizeof(char*));
    if (!h->param_names) { fprintf(stderr, "out of memory\n"); exit(1); }
    for (size_t p = 0; p < route_param_count; p++) {
      h->param_names[p] = (char*)malloc(route_param_name_lens[p] + 1);
      if (!h->param_names[p]) { fprintf(stderr, "out of memory\n"); exit(1); }
      memcpy(h->param_names[p], route_param_names[p], route_param_name_lens[p]);
      h->param_names[p][route_param_name_lens[p]] = '\0';
    }
  }
  /* The route table outlives the closure value passed by the generated caller.
     Retain its environment so scope cleanup cannot leave the handler dangling;
     ku_http_node_free releases this retained ownership exactly once. */
  if (env) ((KuEnvHeader*)env)->retain(env);
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
  free(node->seg);
  for (size_t i = 0; i < node->nh; i++) {
    free(node->handlers[i].method);
    for (size_t p = 0; p < node->handlers[i].nparams; p++) free(node->handlers[i].param_names[p]);
    free(node->handlers[i].param_names);
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
    case 414: return "URI Too Long"; case 417: return "Expectation Failed";
    case 431: return "Request Header Fields Too Large"; case 500: return "Internal Server Error";
    case 501: return "Not Implemented"; case 502: return "Bad Gateway"; case 503: return "Service Unavailable"; case 504: return "Gateway Timeout";
    default: return "Unknown";
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
  v.body = ku_string_static((const uint8_t*)"Internal Server Error", 21);
  v.location = (KuString){0};
  ku_error_drop(&r.error);
  return v;
}
static unsigned long long ku_http_deadline_ms(uint32_t timeout_ms);
static KU_THREAD_LOCAL uint32_t ku_http_write_timeout_ms = 10000;
static int ku_http_set_send_deadline(KuHttpSocket cli, unsigned long long deadline) {
  unsigned long long now = ku_http_now_ms();
  if (now >= deadline) return 0;
  unsigned long long remaining = deadline - now;
  if (remaining > (unsigned long long)UINT32_MAX) remaining = (unsigned long long)UINT32_MAX;
  uint32_t timeout = (uint32_t)remaining;
  if (timeout == 0) timeout = 1;
  return ku_http_socket_set_send_timeout(cli, timeout) == 0;
}
static void ku_http_send_all(KuHttpSocket cli, const char* data, size_t len, unsigned long long deadline) {
  size_t sent = 0;
  while (sent < len) {
    if (!ku_http_set_send_deadline(cli, deadline)) break;
    int n = ku_http_socket_send(cli, data + sent, len - sent);
    if (n < 0 && ku_http_socket_error_interrupted(ku_http_socket_last_error())) continue;
    if (n <= 0) break;
    sent += (size_t)n;
  }
}
/* Send a snprintf-formatted line, clamped to what was actually written.
   snprintf returns the length the output WOULD have had, so passing that return
   value straight to a send() length reads past the end of the buffer. Callers
   here only format bounded numeric lines, but the clamp keeps that guarantee
   local instead of relying on every future caller re-deriving it. */
static void ku_http_send_fmt(KuHttpSocket cli, const char* buf, size_t cap, int written, unsigned long long deadline) {
  if (written <= 0) return;
  size_t len = (size_t)written;
  if (len > cap - 1) len = cap - 1;
  ku_http_send_all(cli, buf, len, deadline);
}
static int ku_http_field_value_safe(const uint8_t* ptr, size_t len) {
  for (size_t i = 0; i < len; i++) {
    uint8_t c = ptr[i];
    if (c != '\t' && (c < 0x20 || c == 0x7f)) return 0;
  }
  return 1;
}
static void ku_http_write_response(KuHttpSocket cli, KuStruct___ku_http_response* resp) {
  unsigned long long write_deadline = ku_http_deadline_ms(ku_http_write_timeout_ms);
  KuStruct___ku_http_response fallback;
  if (!resp || resp->status < 100 || resp->status > 599 ||
      !ku_http_field_value_safe(resp->content_type.ptr, resp->content_type.len) ||
      !ku_http_field_value_safe(resp->location.ptr, resp->location.len)) {
    fallback.status = 500;
    fallback.content_type = ku_string_static((const uint8_t*)"text/plain; charset=utf-8", 25);
    fallback.body = ku_string_static((const uint8_t*)"Internal Server Error", 21);
    fallback.location = (KuString){0};
    resp = &fallback;
  }
  size_t body_len = ((resp->status >= 100 && resp->status < 200) || resp->status == 204 || resp->status == 304) ? 0 : resp->body.len;
  char head[256];
  int hn = snprintf(head, sizeof(head), "HTTP/1.1 %lld %s\r\n", (long long)resp->status, ku_http_status_text(resp->status));
  ku_http_send_fmt(cli, head, sizeof(head), hn, write_deadline);
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
    ku_http_send_all(cli, "content-type: ", 14, write_deadline);
    ku_http_send_all(cli, (const char*)resp->content_type.ptr, resp->content_type.len, write_deadline);
    ku_http_send_all(cli, "\r\n", 2, write_deadline);
  }
  if (resp->location.len) {
    ku_http_send_all(cli, "location: ", 10, write_deadline);
    ku_http_send_all(cli, (const char*)resp->location.ptr, resp->location.len, write_deadline);
    ku_http_send_all(cli, "\r\n", 2, write_deadline);
  }
  int cl = ((resp->status >= 100 && resp->status < 200) || resp->status == 204 || resp->status == 304)
    ? snprintf(head, sizeof(head), "connection: close\r\n\r\n")
    : snprintf(head, sizeof(head), "content-length: %llu\r\nconnection: close\r\n\r\n", (unsigned long long)body_len);
  ku_http_send_fmt(cli, head, sizeof(head), cl, write_deadline);
  if (body_len) ku_http_send_all(cli, (const char*)resp->body.ptr, body_len, write_deadline);
}
static void ku_http_write_status(KuHttpSocket cli, int64_t status, const char* message) {
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
/* `?k=v&k2=v2` -> KuObject{str->str}. Empty parts are skipped; a non-empty
   `=value` part keeps its empty key, and a key without `=` maps to "", matching
   the interpreter's split_path_query. */
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
    if (amp > p) {
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
/* Trie match with static-before-param priority and backtracking. It records only
   captured segment VALUES while walking. Parameter NAMES are attached to the
   final method-specific handler and are applied after a handler is selected;
   shared trie nodes therefore cannot leak one route's names into another route.
   The static-first-with-backtracking walk reproduces the interpreter's
   exact-shape-then-param-scan result (e.g. `/user/me` beats `/user/{id}`). */
static KuHttpHandler* ku_http_trie_match(KuHttpNode* node, const char** segs, size_t* seglens, size_t nseg, size_t i, const char* method, const char** param_values, size_t* param_value_lens, size_t* param_count) {
  if (i == nseg) return ku_http_node_handler(node, method);
  KuHttpNode* sc = ku_http_node_child(node, segs[i], seglens[i]);
  if (sc) {
    size_t saved = *param_count;
    KuHttpHandler* r = ku_http_trie_match(sc, segs, seglens, nseg, i + 1, method, param_values, param_value_lens, param_count);
    if (r) return r;
    *param_count = saved;
  }
  if (node->param) {
    size_t saved = *param_count;
    if (saved >= KU_HTTP_MAX_SEGS) return NULL;
    param_values[saved] = segs[i];
    param_value_lens[saved] = seglens[i];
    *param_count = saved + 1;
    KuHttpHandler* r = ku_http_trie_match(node->param, segs, seglens, nseg, i + 1, method, param_values, param_value_lens, param_count);
    if (r) return r;
    *param_count = saved;
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
static int ku_http_name_eq(const char* ptr, size_t len, const char* expected) {
  size_t n = strlen(expected);
  if (len != n) return 0;
  for (size_t i = 0; i < len; i++) {
    unsigned char c = (unsigned char)ptr[i];
    if (c >= 'A' && c <= 'Z') c = (unsigned char)(c + 32);
    if (c != (unsigned char)expected[i]) return 0;
  }
  return 1;
}
static unsigned long long ku_http_deadline_ms(uint32_t timeout_ms) {
  unsigned long long now = ku_http_now_ms();
  if (~0ULL - now < (unsigned long long)timeout_ms) return ~0ULL;
  return now + (unsigned long long)timeout_ms;
}
/* Wait for readability without making a blocking recv itself time out. Winsock
   documents a stream socket as indeterminate after SO_RCVTIMEO fires; sending a
   408 on that same socket can then fail with WSAECONNABORTED. The platform wait
   wrapper (select on Windows, poll on POSIX) preserves the socket for the
   protocol error response, while the absolute deadline bounds EINTR retries. */
static int ku_http_recv_until(KuHttpSocket cli, char* buffer, int length, unsigned long long deadline) {
  for (;;) {
    int selected = ku_http_socket_wait_readable(cli, deadline);
    if (selected <= 0) return -1;
    int received = ku_http_socket_recv(cli, buffer, (size_t)length);
    if (received < 0 && ku_http_socket_error_interrupted(ku_http_socket_last_error())) continue;
    return received;
  }
}
/* Stage 8e: read and validate one request under the same request-level limits the
   interpreter enforces (read_http_request / read_http_header). Header bytes are
   received in bounded chunks, then validated byte-by-byte so the \r\n\r\n
   boundary, the 431 cutoff, and the idle->read-header timeout hand-off retain
   their strict semantics without one select()+recv() pair per byte. Bytes after
   the terminator in the same recv are retained as the body prefix. Body bytes are
   read under read_body_timeout. On any wire/limit violation this answers with the
   interpreter's status (400/408/413/431) and returns. Every malloc'd buffer is
   freed on every path. */
static void ku_http_handle_connection(KuHttpServer* server, KuHttpSocket cli) {
  long long max_header = server->max_header_bytes > 0 ? server->max_header_bytes : (16 * 1024);
  long long max_body = server->max_body_bytes >= 0 ? server->max_body_bytes : 1000000;
  uint32_t idle_tmo = (uint32_t)(server->idle_timeout_ms > 0 ? server->idle_timeout_ms : 5000);
  uint32_t hdr_tmo = (uint32_t)(server->read_header_timeout_ms > 0 ? server->read_header_timeout_ms : 5000);
  uint32_t body_tmo = (uint32_t)(server->read_body_timeout_ms > 0 ? server->read_body_timeout_ms : 10000);
  /* ku_http_write_response applies write_timeout_ms as one total deadline across
     every status/header/body send, matching the interpreter and preventing a
     slow reader from refreshing a full timeout after each partial write. */
  ku_http_write_timeout_ms = (uint32_t)(server->write_timeout_ms > 0 ? server->write_timeout_ms : 10000);
  /* Header read: bounded chunks into a growable buffer. First byte waits up to
     idle_timeout; subsequent chunks share one absolute read_header_timeout.
     Validation below still advances byte-by-byte, preserving the exact error
     precedence: byte max_header+1 is 431 even if that byte is malformed. */
  size_t cap = 1024, buffered = 0, hlen = 0;
  size_t body_prefix_start = 0, body_prefix_len = 0;
  char* hdr = (char*)malloc(cap);
  if (!hdr) { ku_http_write_status(cli, 500, "Internal Server Error"); return; }
  unsigned long long idle_deadline = ku_http_deadline_ms(idle_tmo);
  unsigned long long header_deadline = 0;
  int header_done = 0;
  for (;;) {
    char incoming[KU_HTTP_READ_CHUNK];
    int n = ku_http_recv_until(cli, incoming, KU_HTTP_READ_CHUNK,
                               header_deadline ? header_deadline : idle_deadline);
    if (n == 0) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
    if (n < 0) { free(hdr); ku_http_write_status(cli, 408, "Request Timeout"); return; }
    if ((size_t)n > SIZE_MAX - buffered) { free(hdr); ku_http_write_status(cli, 500, "Internal Server Error"); return; }
    size_t scan_from = buffered;
    size_t needed = buffered + (size_t)n;
    if (needed > cap) {
      size_t new_cap = cap;
      while (new_cap < needed) {
        if (new_cap > SIZE_MAX / 2) { new_cap = needed; break; }
        new_cap *= 2;
      }
      char* nb = (char*)realloc(hdr, new_cap);
      if (!nb) { free(hdr); ku_http_write_status(cli, 500, "Internal Server Error"); return; }
      hdr = nb; cap = new_cap;
    }
    memcpy(hdr + buffered, incoming, (size_t)n);
    buffered = needed;
    if (scan_from == 0) header_deadline = ku_http_deadline_ms(hdr_tmo);
    for (size_t i = scan_from; i < buffered; i++) {
      char c = hdr[i];
      if ((long long)(i + 1) > max_header) { free(hdr); ku_http_write_status(cli, 431, "Request Header Fields Too Large"); return; }
      if (c == '\n' && (i < 1 || hdr[i - 1] != '\r')) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
      if (i >= 1 && hdr[i - 1] == '\r' && c != '\n') { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
      if (i >= 3 && hdr[i-3] == '\r' && hdr[i-2] == '\n' && hdr[i-1] == '\r' && hdr[i] == '\n') {
        hlen = i - 3;
        body_prefix_start = i + 1;
        body_prefix_len = buffered - body_prefix_start;
        header_done = 1;
        break;
      }
    }
    if (header_done) break;
  }
  if (!header_done || hlen == 0) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  /* The interpreter decodes the complete header block as UTF-8 before parsing.
     Enforce the same boundary here so invalid field values never become KuString
     instances or reach a handler. The body prefix is validated separately after
     Content-Length is applied. */
  if (!ku_http_utf8_valid((const uint8_t*)hdr, hlen)) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  size_t fl_end = hlen;
  for (size_t i = 0; i + 1 < hlen; i++) { if (hdr[i] == '\r' && hdr[i + 1] == '\n') { fl_end = i; break; } }
  size_t sp1 = fl_end, sp2 = fl_end;
  for (size_t i = 0; i < fl_end; i++) if (hdr[i] == ' ') { sp1 = i; break; }
  if (sp1 < fl_end) for (size_t i = sp1 + 1; i < fl_end; i++) if (hdr[i] == ' ') { sp2 = i; break; }
  if (sp1 == 0 || sp1 >= fl_end || sp2 <= sp1 + 1 || sp2 + 1 >= fl_end) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  for (size_t i = sp2 + 1; i < fl_end; i++) if (hdr[i] == ' ') { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  size_t method_len = sp1;
  size_t target_start = sp1 + 1, target_len = sp2 - target_start;
  size_t version_start = sp2 + 1, version_len = fl_end - version_start;
  if (method_len > 32) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  for (size_t i = 0; i < method_len; i++) if (!ku_http_is_tchar((unsigned char)hdr[i])) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  if (version_len != 8 || memcmp(hdr + version_start, "HTTP/1.1", 8) != 0) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  if (!ku_http_valid_origin_form((const uint8_t*)hdr + target_start, target_len)) {
    free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return;
  }
  int host_count = 0, content_length_count = 0;
  { size_t i = (fl_end < hlen) ? fl_end + 2 : hlen;
    while (i < hlen) {
      size_t ls = i;
      size_t le = hlen;
      for (size_t k = i; k + 1 < hlen; k++) { if (hdr[k] == '\r' && hdr[k + 1] == '\n') { le = k; break; } }
      if (le > ls) {
        if (hdr[ls] == ' ' || hdr[ls] == '\t') { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
        size_t colon = le;
        for (size_t k = ls; k < le; k++) if (hdr[k] == ':') { colon = k; break; }
        if (colon == ls || colon >= le) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
        for (size_t k = ls; k < colon; k++) if (!ku_http_is_tchar((unsigned char)hdr[k])) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
        size_t vs = colon + 1, ve = le;
        for (size_t k = vs; k < ve; k++) {
          unsigned char c = (unsigned char)hdr[k];
          if (c != '\t' && (c < 0x20 || c == 0x7f)) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
        }
        while (vs < ve && (hdr[vs] == ' ' || hdr[vs] == '\t')) vs++;
        while (ve > vs && (hdr[ve - 1] == ' ' || hdr[ve - 1] == '\t')) ve--;
        if (ku_http_name_eq(hdr + ls, colon - ls, "host")) {
          host_count++;
          if (host_count != 1 || vs == ve) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
        } else if (ku_http_name_eq(hdr + ls, colon - ls, "transfer-encoding")) {
          free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return;
        } else if (ku_http_name_eq(hdr + ls, colon - ls, "expect")) {
          free(hdr); ku_http_write_status(cli, 417, "Expectation Failed"); return;
        } else if (ku_http_name_eq(hdr + ls, colon - ls, "content-length")) {
          content_length_count++;
          if (content_length_count != 1 || vs == ve) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
          for (size_t k = vs; k < ve; k++) if (hdr[k] < '0' || hdr[k] > '9') { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
        }
      }
      if (le >= hlen) break;
      i = le + 2;
    }
  }
  if (host_count != 1) { free(hdr); ku_http_write_status(cli, 400, "Bad Request"); return; }
  char method[33] = {0};
  memcpy(method, hdr, method_len); method[method_len] = '\0';
  /* The target is length-checked BEFORE it is copied and before any routing, so an
     over-long target can never be truncated into a shorter one that matches a real
     route, and never reaches a handler. */
  if (target_len > (size_t)KU_HTTP_MAX_TARGET) { free(hdr); ku_http_write_status(cli, 414, "URI Too Long"); return; }
  char target[KU_HTTP_MAX_TARGET + 1] = {0};
  memcpy(target, hdr + target_start, target_len); target[target_len] = '\0';
  /* Header map (lowercased names, trimmed values) matching the interpreter, plus a
     strict content-length parse: present-but-non-numeric -> 400. */
  KuObject* headers = ku_http_parse_headers(hdr, (int)hlen);
  /* Content-Length is strict RFC decimal: one or more digits, no sign. Duplicate
     fields and Transfer-Encoding were rejected before the map was built.

     The accumulator is unsigned WITH an explicit overflow guard. A signed
     accumulator wraps on a huge value (undefined behaviour), and the wrapped
     negative result would then slip past the 413 check and reach the handler with
     an empty body and a 200 -- where the interpreter answers 400. */
  unsigned long long content_length = 0;
  { KuValue* cv = ku_object_get(headers, ku_string_static((const uint8_t*)"content-length", 14));
    if (cv && cv->tag == KU_STR) {
      KuString s = cv->as.s;
      unsigned long long v = 0; int ok = 1; size_t k = 0;
      if (s.len == 0) ok = 0;
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
  char* qmark = strchr(target, '?');
  size_t path_len = qmark ? (size_t)(qmark - target) : strlen(target);
  size_t request_segments = 0;
  { size_t i = 0;
    while (i < path_len) {
      while (i < path_len && target[i] == '/') i++;
      size_t start = i;
      while (i < path_len && target[i] != '/') i++;
      if (i > start && ++request_segments > (size_t)KU_HTTP_MAX_SEGS) {
        ku_object_drop(headers); free(hdr); ku_http_write_status(cli, 414, "URI Too Long"); return;
      }
    }
  }
  /* Body: read exactly content_length bytes under read_body_timeout. A timeout or
     peer close before the body completes -> 408 (matches read_exact -> 408). */
  char* body_buf = NULL; size_t body_len = 0;
  if (content_length > 0) {
    body_buf = (char*)malloc((size_t)content_length);
    if (!body_buf) { ku_object_drop(headers); free(hdr); ku_http_write_status(cli, 500, "Internal Server Error"); return; }
    unsigned long long body_deadline = ku_http_deadline_ms(body_tmo);
    size_t got = body_prefix_len;
    if (got > (size_t)content_length) got = (size_t)content_length;
    if (got > 0) memcpy(body_buf, hdr + body_prefix_start, got);
    while (got < (size_t)content_length) {
      /* recv takes an int length, and max_body_bytes is a config value that may
         exceed INT_MAX -- chunk the read instead of overflowing the cast. */
      size_t want = (size_t)content_length - got;
      if (want > (size_t)1048576) want = (size_t)1048576;
      int n = ku_http_recv_until(cli, body_buf + got, (int)want, body_deadline);
      if (n <= 0) { free(body_buf); ku_object_drop(headers); free(hdr); ku_http_write_status(cli, 408, "Request Timeout"); return; }
      got += (size_t)n;
    }
    body_len = (size_t)content_length;
  }
  if (!ku_http_utf8_valid((const uint8_t*)body_buf, body_len)) {
    free(body_buf); ku_object_drop(headers); free(hdr);
    ku_http_write_status(cli, 400, "Bad Request"); return;
  }
  /* Sized off the target limit, not a smaller fixed buffer: normalization only
     ever adds a leading '/', so this cannot truncate a target that passed the 414
     check -- and a truncated normalized path would reintroduce exactly the route
     collision the 414 check exists to prevent. */
  char norm[KU_HTTP_MAX_TARGET + 2];
  ku_http_normalize_path(target, path_len, norm, sizeof(norm));
  const char* segs[KU_HTTP_MAX_SEGS]; size_t seglens[KU_HTTP_MAX_SEGS]; size_t nseg = 0;
  { size_t nl = strlen(norm); size_t j = 0;
    while (j < nl) {
      while (j < nl && norm[j] == '/') j++;
      size_t st = j;
      while (j < nl && norm[j] != '/') j++;
      if (j > st) {
        if (nseg >= KU_HTTP_MAX_SEGS) { ku_object_drop(headers); free(body_buf); free(hdr); ku_http_write_status(cli, 414, "URI Too Long"); return; }
        segs[nseg] = norm + st; seglens[nseg] = j - st; nseg++;
      }
    }
  }
  const char* param_values[KU_HTTP_MAX_SEGS];
  size_t param_value_lens[KU_HTTP_MAX_SEGS];
  size_t param_count = 0;
  KuHttpHandler* route = ku_http_trie_match(server->root, segs, seglens, nseg, 0, method, param_values, param_value_lens, &param_count);
  if (route && param_count != route->nparams) {
    ku_object_drop(headers); free(body_buf); free(hdr);
    ku_http_write_status(cli, 500, "Internal Server Error"); return;
  }
  if (route) {
    KuStruct___ku_http_response resp = (KuStruct___ku_http_response){0};
    int handler_timed_out = 0;
    if (route->arity == 1) {
      KuObject* params = ku_object_new(0);
      for (size_t p = 0; p < route->nparams; p++) {
        KuString key = ku_http_string_copy(route->param_names[p], strlen(route->param_names[p]));
        KuString val = ku_http_string_copy(param_values[p], param_value_lens[p]);
        ku_object_set(params, key, ku_v_str(val));
      }
      KuStruct___ku_http_request req;
      req.method = ku_http_string_copy(method, strlen(method));
      req.path = ku_http_string_copy(target, path_len);
      req.body = ku_http_string_copy(body_buf ? body_buf : "", body_len);
      req.params = params;
      req.query = ku_http_parse_query(target, path_len);
      req.headers = headers; headers = NULL;
      if (route->returns_result) {
        __ku_handler_timeout_begin((unsigned long long)server->handler_timeout_ms);
        KuResult_struct___ku_http_response rr = ((KuResult_struct___ku_http_response(*)(void*, KuStruct___ku_http_request))route->invoke)(route->env, req);
        handler_timed_out = __ku_handler_timeout_finish();
        if (handler_timed_out) ku_result_drop_struct___ku_http_response(&rr);
        else resp = ku_http_response_from_result(rr);
      } else {
        __ku_handler_timeout_begin((unsigned long long)server->handler_timeout_ms);
        resp = ((KuStruct___ku_http_response(*)(void*, KuStruct___ku_http_request))route->invoke)(route->env, req);
        handler_timed_out = __ku_handler_timeout_finish();
      }
    } else {
      if (route->returns_result) {
        __ku_handler_timeout_begin((unsigned long long)server->handler_timeout_ms);
        KuResult_struct___ku_http_response rr = ((KuResult_struct___ku_http_response(*)(void*))route->invoke)(route->env);
        handler_timed_out = __ku_handler_timeout_finish();
        if (handler_timed_out) ku_result_drop_struct___ku_http_response(&rr);
        else resp = ku_http_response_from_result(rr);
      } else {
        __ku_handler_timeout_begin((unsigned long long)server->handler_timeout_ms);
        resp = ((KuStruct___ku_http_response(*)(void*))route->invoke)(route->env);
        handler_timed_out = __ku_handler_timeout_finish();
      }
    }
    if (handler_timed_out) {
      /* The worker owns this socket and is the sole response writer. A plain
         handler may have completed just after its deadline with a real response;
         drop it before replacing it with 504. A timed-out Result was dropped in
         its branch above. */
      if (!route->returns_result) ku_drop_struct___ku_http_response(&resp);
      ku_http_write_status(cli, 504, "Gateway Timeout");
    } else {
      ku_http_write_response(cli, &resp);
      ku_drop_struct___ku_http_response(&resp);
    }
  } else {
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
     * handler_timeout_ms -> one thread-local absolute deadline per worker. Native
       IR polls it on while back-edges and after Ku/closure calls; array.map polls
       around every mapper invocation. A timeout follows explicit return edges, so
       finally blocks execute and ordinary frame cleanup releases owned values.
       Safepoints give a timed-out frame a fixed one-second grace window for its
       finally route, including helper calls made there. Once that cleanup grace
       expires, the next safepoint diverts an infinite finally to the timeout
       exit; a blocking native/FFI call with no safepoint remains uninterruptible.
       The worker that invoked the handler remains the only socket owner and emits
       the single 504 response after the call stack has unwound -- there is no
       watchdog, forced thread termination, concurrent close, or concurrent write.

   The permit counter is incremented only by the single acceptor thread and
   decremented by whichever thread finishes the connection (acceptor on a reject,
   worker after handling). Interlocked operations on Windows and C11 atomics on
   POSIX keep the bound race-free. The routing trie is built before listen and
   never mutated, so all workers read it lock-free. */
#define KU_HTTP_WORKER_CAP 64
#define KU_HTTP_ACCEPT_PEER_BACKOFF_CAP_MS 8
#define KU_HTTP_ACCEPT_RESOURCE_RETRY_CAP 8
#define KU_HTTP_ACCEPT_RESOURCE_BACKOFF_STEP_MS 10
/* Process-global connection-permit counter (live accepted-but-not-yet-finished
   connections). Only one server runs per process, so a file-scope global is
   safe; it is touched from the acceptor and every worker through the platform
   atomic wrappers above. */
static KuHttpAtomicCounter ku_http_active_conns;
/* Bounded hand-off queue of client sockets: one producer (the acceptor) pushes,
   N workers pop. A platform mutex guards the ring buffer and a condition
   variable parks idle workers; `closed` wakes every worker. Normal bounded-test
   shutdown drains queued work, while a fatal accept failure discards work that
   has not started before workers exit. */
#if defined(_WIN32)
typedef CRITICAL_SECTION KuHttpMutex;
typedef CONDITION_VARIABLE KuHttpCondition;
static void ku_http_sync_init(KuHttpMutex* mutex, KuHttpCondition* condition) {
  InitializeCriticalSection(mutex); InitializeConditionVariable(condition);
}
static void ku_http_sync_destroy(KuHttpMutex* mutex, KuHttpCondition* condition) {
  (void)condition; DeleteCriticalSection(mutex);
}
static void ku_http_mutex_lock(KuHttpMutex* mutex) { EnterCriticalSection(mutex); }
static void ku_http_mutex_unlock(KuHttpMutex* mutex) { LeaveCriticalSection(mutex); }
static void ku_http_condition_wait(KuHttpCondition* condition, KuHttpMutex* mutex) {
  if (!SleepConditionVariableCS(condition, mutex, INFINITE)) {
    fputs("http queue condition wait failed\n", stderr); exit(1);
  }
}
static void ku_http_condition_signal(KuHttpCondition* condition) { WakeConditionVariable(condition); }
static void ku_http_condition_broadcast(KuHttpCondition* condition) { WakeAllConditionVariable(condition); }
#else
typedef pthread_mutex_t KuHttpMutex;
typedef pthread_cond_t KuHttpCondition;
static void ku_http_sync_init(KuHttpMutex* mutex, KuHttpCondition* condition) {
  if (pthread_mutex_init(mutex, NULL) != 0) {
    fputs("http queue mutex initialization failed\n", stderr); exit(1);
  }
  if (pthread_cond_init(condition, NULL) != 0) {
    pthread_mutex_destroy(mutex);
    fputs("http queue condition initialization failed\n", stderr); exit(1);
  }
}
static void ku_http_sync_destroy(KuHttpMutex* mutex, KuHttpCondition* condition) {
  if (pthread_cond_destroy(condition) != 0 || pthread_mutex_destroy(mutex) != 0) {
    fputs("http queue synchronization destroy failed\n", stderr); exit(1);
  }
}
static void ku_http_mutex_lock(KuHttpMutex* mutex) {
  if (pthread_mutex_lock(mutex) != 0) { fputs("http queue mutex lock failed\n", stderr); exit(1); }
}
static void ku_http_mutex_unlock(KuHttpMutex* mutex) {
  if (pthread_mutex_unlock(mutex) != 0) { fputs("http queue mutex unlock failed\n", stderr); exit(1); }
}
static void ku_http_condition_wait(KuHttpCondition* condition, KuHttpMutex* mutex) {
  if (pthread_cond_wait(condition, mutex) != 0) {
    fputs("http queue condition wait failed\n", stderr); exit(1);
  }
}
static void ku_http_condition_signal(KuHttpCondition* condition) {
  if (pthread_cond_signal(condition) != 0) { fputs("http queue condition signal failed\n", stderr); exit(1); }
}
static void ku_http_condition_broadcast(KuHttpCondition* condition) {
  if (pthread_cond_broadcast(condition) != 0) { fputs("http queue condition broadcast failed\n", stderr); exit(1); }
}
#endif
typedef struct {
  KuHttpSocket* items; int cap; int head; int tail; int count; int closed;
  KuHttpMutex lock; KuHttpCondition not_empty;
} KuHttpQueue;
static void ku_http_queue_init(KuHttpQueue* q, int cap) {
  if (cap < 1) cap = 1;
  q->items = (KuHttpSocket*)malloc((size_t)cap * sizeof(KuHttpSocket));
  if (!q->items) { fprintf(stderr, "out of memory\n"); exit(1); }
  q->cap = cap; q->head = 0; q->tail = 0; q->count = 0; q->closed = 0;
  ku_http_sync_init(&q->lock, &q->not_empty);
}
/* Non-blocking push; returns 0 when the queue is full (pending exhausted). */
static int ku_http_queue_push(KuHttpQueue* q, KuHttpSocket s) {
  int ok = 0;
  ku_http_mutex_lock(&q->lock);
  if (!q->closed && q->count < q->cap) {
    q->items[q->tail] = s; q->tail = (q->tail + 1) % q->cap; q->count++;
    ku_http_condition_signal(&q->not_empty); ok = 1;
  }
  ku_http_mutex_unlock(&q->lock);
  return ok;
}
/* Blocking pop; returns KU_HTTP_INVALID_SOCKET when the queue is closed and
   empty, which is the worker's signal to exit. */
static KuHttpSocket ku_http_queue_pop(KuHttpQueue* q) {
  KuHttpSocket s = KU_HTTP_INVALID_SOCKET;
  ku_http_mutex_lock(&q->lock);
  while (q->count == 0 && !q->closed)
    ku_http_condition_wait(&q->not_empty, &q->lock);
  if (q->count > 0) {
    s = q->items[q->head]; q->head = (q->head + 1) % q->cap; q->count--;
  }
  ku_http_mutex_unlock(&q->lock);
  return s;
}
static void ku_http_queue_close(KuHttpQueue* q, int drain) {
  ku_http_mutex_lock(&q->lock);
  q->closed = 1;
  if (!drain) {
    while (q->count > 0) {
      KuHttpSocket s = q->items[q->head];
      q->head = (q->head + 1) % q->cap;
      q->count--;
      ku_http_socket_close(s);
      ku_http_atomic_decrement(&ku_http_active_conns);
    }
    q->tail = q->head;
  }
  ku_http_condition_broadcast(&q->not_empty);
  ku_http_mutex_unlock(&q->lock);
}
/* Free the queue after workers have joined. Any still-buffered sockets are a
   defensive cleanup fallback; close them and roll back their permits. */
static void ku_http_queue_free(KuHttpQueue* q) {
  while (q->count > 0) {
    KuHttpSocket s = q->items[q->head]; q->head = (q->head + 1) % q->cap; q->count--;
    ku_http_socket_close(s);
    ku_http_atomic_decrement(&ku_http_active_conns);
  }
  ku_http_sync_destroy(&q->lock, &q->not_empty);
  free(q->items); q->items = NULL;
}
typedef struct { KuHttpServer* server; KuHttpQueue* queue; } KuHttpWorkerCtx;
#if defined(_WIN32)
static unsigned __stdcall ku_http_worker(void* arg) {
#else
static void* ku_http_worker(void* arg) {
#endif
  KuHttpWorkerCtx* ctx = (KuHttpWorkerCtx*)arg;
  for (;;) {
    KuHttpSocket cli = ku_http_queue_pop(ctx->queue);
    if (cli == KU_HTTP_INVALID_SOCKET) break;      /* queue closed and empty */
    ku_http_handle_connection(ctx->server, cli);
    ku_http_socket_shutdown_write(cli);
    ku_http_socket_close(cli);
    ku_http_atomic_decrement(&ku_http_active_conns); /* release permit */
  }
#if defined(KU_NATIVE_RUNTIME_MYSQL)
  ku_mysql_thread_shutdown();
#endif
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}
/* Answer an over-limit connection with 503 and release its permit. Shared by the
   max_connections and max_pending rejection paths so the close + decrement stay
   in one place. */
static void ku_http_reject_503(KuHttpServer* server, KuHttpSocket cli) {
  /* Mirrors the interpreter's reject_http_connection: drain the in-flight request
     under one 50ms total deadline, bounded by max_header_bytes and stopping at
     the header terminator, then write 503 and half-close.

     Draining BEFORE the write matters: closing a socket with unread request
     bytes can abort with an RST, which discards the 503 (the client sees a
     connection reset instead of the status). Bounding the drain matters too: an
     unbounded `while (recv(..) > 0)` lets a peer that keeps streaming pin this
     thread indefinitely. */
  long long max_header = server->max_header_bytes > 0 ? server->max_header_bytes : (16 * 1024);
  unsigned long long drain_deadline = ku_http_deadline_ms(50);
  { char drain[1024]; long long received = 0; unsigned char w[4] = {0, 0, 0, 0}; long long seen = 0; int done = 0;
    while (received < max_header && !done) {
      int n = ku_http_recv_until(cli, drain, (int)sizeof(drain), drain_deadline);
      if (n <= 0) break;
      received += (long long)n;
      for (int i = 0; i < n && !done; i++) {
        w[0] = w[1]; w[1] = w[2]; w[2] = w[3]; w[3] = (unsigned char)drain[i]; seen++;
        if (seen >= 4 && w[0] == '\r' && w[1] == '\n' && w[2] == '\r' && w[3] == '\n') done = 1;
        else if (seen >= 2 && w[2] == '\n' && w[3] == '\n') done = 1;
      }
    }
  }
  ku_http_write_timeout_ms = (uint32_t)(server->write_timeout_ms > 0 ? server->write_timeout_ms : 10000);
  ku_http_write_status(cli, 503, "Service Unavailable");
  ku_http_socket_shutdown_write(cli);
  ku_http_socket_close(cli);
  ku_http_atomic_decrement(&ku_http_active_conns);
}
static int ku_http_parse_ipv4(const char* text, unsigned long* network_order) {
  unsigned long octets[4] = {0, 0, 0, 0};
  size_t part = 0;
  const char* p = text;
  while (part < 4) {
    if (*p < '0' || *p > '9') return 0;
    unsigned long value = 0;
    size_t digits = 0;
    while (*p >= '0' && *p <= '9') {
      value = value * 10UL + (unsigned long)(*p - '0');
      if (value > 255UL || ++digits > 3) return 0;
      p++;
    }
    octets[part++] = value;
    if (part < 4) {
      if (*p != '.') return 0;
      p++;
    }
  }
  if (*p != '\0') return 0;
  unsigned long host_order = (octets[0] << 24) | (octets[1] << 16) |
                             (octets[2] << 8) | octets[3];
  *network_order = htonl(host_order);
  return 1;
}
static KuResult_null ku_http_listen(KuHttpServer* server, KuString address) {
  ku_http_validate_server(server);
  if (address.len == 0 || memchr(address.ptr, 0, address.len) != NULL) {
    ku_string_drop(&address);
    return ku_http_listen_err(server, "invalid listen address");
  }
  char* addr = ku_string_to_cstr(address);
  ku_string_drop(&address);
  unsigned long host = htonl(INADDR_LOOPBACK);
  unsigned long port = 0;
  char* colon = strchr(addr, ':');
  int valid_address = colon != NULL && strchr(colon + 1, ':') == NULL;
  if (valid_address) {
    *colon = '\0';
    const char* port_text = colon + 1;
    if (*port_text == '\0') valid_address = 0;
    while (valid_address && *port_text) {
      if (*port_text < '0' || *port_text > '9') { valid_address = 0; break; }
      port = port * 10UL + (unsigned long)(*port_text - '0');
      if (port > 65535UL) { valid_address = 0; break; }
      port_text++;
    }
    if (valid_address && addr[0] != '\0' && strcmp(addr, "localhost") != 0 &&
        !ku_http_parse_ipv4(addr, &host)) valid_address = 0;
  }
  if (!valid_address) {
    free(addr);
    return ku_http_listen_err(server, "invalid listen address");
  }
  free(addr);
  if (ku_http_net_init() != 0) return ku_http_listen_err(server, "network initialization failed");
  KuHttpSocket srv = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (srv == KU_HTTP_INVALID_SOCKET) { ku_http_net_cleanup(); return ku_http_listen_err(server, "socket failed"); }
  int yes = 1;
  setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, (const char*)&yes, sizeof(yes));
  struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET; sa.sin_addr.s_addr = host; sa.sin_port = htons((unsigned short)port);
  if (bind(srv, (struct sockaddr*)&sa, sizeof(sa)) != 0) { ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "bind failed"); }
  if (listen(srv, SOMAXCONN) != 0) { ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "listen failed"); }
  long max_requests = 0;
  { const char* mr = getenv("KU_HTTP_MAX_REQUESTS"); if (mr) max_requests = atol(mr); }
  /* Resolve admission-control limits from the server config (defaults already
     applied in ku_http_server_new). */
  /* The limits are i64 (as in the interpreter), but Windows uses a 32-bit LONG
     and the queue/worker counts are ints -- clamp instead of casting, so a
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
  ku_http_atomic_store(&ku_http_active_conns, 0);
  KuHttpQueue queue; ku_http_queue_init(&queue, max_pending);
  KuHttpWorkerCtx ctx; ctx.server = server; ctx.queue = &queue;
#if defined(_WIN32)
  HANDLE* workers = (HANDLE*)malloc((size_t)nworkers * sizeof(HANDLE));
#else
  pthread_t* workers = (pthread_t*)malloc((size_t)nworkers * sizeof(pthread_t));
#endif
  int spawned = 0;
  if (workers) {
    for (int w = 0; w < nworkers; w++) {
#if defined(_WIN32)
      uintptr_t h = _beginthreadex(NULL, 0, ku_http_worker, &ctx, 0, NULL);
      if (h == 0) break;
      workers[spawned++] = (HANDLE)h;
#else
      pthread_t worker;
      if (pthread_create(&worker, NULL, ku_http_worker, &ctx) != 0) break;
      workers[spawned++] = worker;
#endif
    }
  }
  /* Acceptor loop. Blocking accept() on this thread means listen() blocks here
     until the process is killed (resident server) or KU_HTTP_MAX_REQUESTS
     connections have been accepted (finite smoke/ASan runs). */
  long accepted = 0;
  int peer_accept_errors = 0;
  int resource_accept_errors = 0;
  const char* listen_failure = NULL;
  for (;;) {
    KuHttpSocket cli = accept(srv, NULL, NULL);
    if (cli == KU_HTTP_INVALID_SOCKET) {
      int accept_error = ku_http_socket_last_error();
      if (ku_http_socket_error_accept_peer_transient(accept_error)) {
        if (peer_accept_errors < KU_HTTP_ACCEPT_PEER_BACKOFF_CAP_MS)
          peer_accept_errors++;
        resource_accept_errors = 0;
        ku_http_accept_retry_delay((uint32_t)peer_accept_errors);
        continue;
      }
      peer_accept_errors = 0;
      if (ku_http_socket_error_accept_resource_pressure(accept_error) &&
          resource_accept_errors < KU_HTTP_ACCEPT_RESOURCE_RETRY_CAP) {
        resource_accept_errors++;
        ku_http_accept_retry_delay(
          (uint32_t)resource_accept_errors * KU_HTTP_ACCEPT_RESOURCE_BACKOFF_STEP_MS);
        continue;
      }
      listen_failure = "accept failed";
      break;
    }
    peer_accept_errors = 0;
    resource_accept_errors = 0;
    if (ku_http_socket_suppress_sigpipe(cli) != 0) {
      ku_http_socket_close(cli);
      listen_failure = "accepted socket configuration failed";
      break;
    }
    if (max_requests > 0) accepted++;
    long live = ku_http_atomic_increment(&ku_http_active_conns); /* connection permit */
    if (live > max_conn) {
      ku_http_reject_503(server, cli);            /* max_connections exceeded */
    } else if (spawned == 0) {
      /* Degenerate fallback: no worker threads could be spawned, so serve inline
         on the acceptor. Still correct (serial), permit released after handling. */
      ku_http_handle_connection(server, cli);
      ku_http_socket_shutdown_write(cli);
      ku_http_socket_close(cli);
      ku_http_atomic_decrement(&ku_http_active_conns);
    } else if (!ku_http_queue_push(&queue, cli)) {
      ku_http_reject_503(server, cli);            /* max_pending exceeded */
    }
    if (max_requests > 0 && accepted >= max_requests) break;
  }
  ku_http_socket_close(srv);
  /* A normal KU_HTTP_MAX_REQUESTS exit drains accepted work for deterministic
     tests. A fatal accept/configuration failure closes queued sockets that have
     not started, avoiding a long shutdown proportional to max_pending_requests.
     In-flight handlers still finish cooperatively before join; pthreads cannot
     safely terminate a worker blocked inside an arbitrary native/FFI call. */
  ku_http_queue_close(&queue, listen_failure == NULL);
  if (spawned > 0) {
#if defined(_WIN32)
    if (WaitForMultipleObjects((DWORD)spawned, workers, TRUE, INFINITE) == WAIT_FAILED) {
      fputs("http worker join failed\n", stderr); exit(1);
    }
    for (int w = 0; w < spawned; w++) CloseHandle(workers[w]);
#else
    for (int w = 0; w < spawned; w++) {
      if (pthread_join(workers[w], NULL) != 0) {
        fputs("http worker join failed\n", stderr); exit(1);
      }
    }
#endif
  }
  free(workers);
  ku_http_queue_free(&queue);
  ku_http_net_cleanup();
  if (listen_failure) return ku_http_listen_err(server, listen_failure);
  ku_http_server_free(server);
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
fn emit_array_try_get_helpers(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
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
fn emit_string_slice_helper(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
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

/// Materialize Unicode scalar values once for parsers and other sequential text
/// consumers. ASCII scalars use immortal static bytes; other scalars own their
/// UTF-8 bytes. Neither representation borrows the input, so the array remains
/// valid after the receiver is dropped and uses the ordinary array/string ABI.
fn emit_string_chars_helper(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    if !program_uses_intrinsic(program, "string.chars") {
        return Ok(());
    }
    out.push_str(
        "static KuArray_str ku_string_chars(KuString s) {\n\
         \x20 static const uint8_t ascii[128] = {\n\
         \x20   0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,\n\
         \x20   16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,\n\
         \x20   32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,\n\
         \x20   48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63,\n\
         \x20   64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79,\n\
         \x20   80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95,\n\
         \x20   96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111,\n\
         \x20   112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127\n\
         \x20 };\n\
         \x20 KuArray_str result = { ku_string_char_len(s), NULL };\n\
         \x20 if (result.len == 0) return result;\n\
         \x20 if (result.len > SIZE_MAX / sizeof(KuString)) { fprintf(stderr, \"string.chars result is too large\\n\"); exit(1); }\n\
         \x20 result.data = (KuString*)calloc(result.len, sizeof(KuString));\n\
         \x20 if (!result.data) { fprintf(stderr, \"string.chars allocation failed\\n\"); exit(1); }\n\
         \x20 size_t byte = 0, index = 0;\n\
         \x20 while (byte < s.len) {\n\
         \x20   size_t next = byte + 1;\n\
         \x20   while (next < s.len && (s.ptr[next] & 0xC0) == 0x80) next++;\n\
         \x20   size_t len = next - byte;\n\
         \x20   if (len == 1 && s.ptr[byte] < 128) {\n\
         \x20     result.data[index] = ku_string_static(ascii + s.ptr[byte], 1);\n\
         \x20   } else {\n\
         \x20     result.data[index] = ku_string_alloc(len);\n\
         \x20     memcpy(result.data[index].ptr, s.ptr + byte, len);\n\
         \x20   }\n\
         \x20   index++; byte = next;\n\
         \x20 }\n\
         \x20 return result;\n\
         }\n\n",
    );
    Ok(())
}

fn program_uses_intrinsic(program: &IrProgram, intrinsic: &str) -> bool {
    program.functions.iter().any(|function| {
        function.blocks.iter().any(|block| {
            let mut used = false;
            for inst in &block.instructions {
                walk_inst_exprs(inst, &mut |expr| {
                    if expr_uses_intrinsic(expr, intrinsic) {
                        used = true;
                    }
                });
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                if expr_uses_intrinsic(expr, intrinsic) {
                    used = true;
                }
            });
            used
        })
    })
}

/// Native TLS is an opt-in target-pack feature. Keep ordinary TCP artifacts
/// lean by emitting its standalone linker marker only when a lowered object
/// literal can enable TLS (`tls: false` alone is compile-time disabled) and the
/// program actually calls `net.client`. A wholly dynamic object assembled
/// outside the visible IR remains fail-closed at runtime with `tls_unavailable`.
fn program_mentions_native_tls_config(program: &IrProgram) -> bool {
    if !program_uses_intrinsic(program, "net.client") {
        return false;
    }
    program.functions.iter().any(|function| {
        function.blocks.iter().any(|block| {
            let mut mentioned = false;
            for inst in &block.instructions {
                walk_inst_exprs(inst, &mut |expr| {
                    if expr_mentions_native_tls_config(expr) {
                        mentioned = true;
                    }
                });
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                if expr_mentions_native_tls_config(expr) {
                    mentioned = true;
                }
            });
            mentioned
        })
    })
}

fn expr_mentions_native_tls_config(expr: &IrExpr) -> bool {
    if let IrExprKind::Call {
        kind: IrCallKind::Intrinsic(name),
        args,
        ..
    } = &expr.kind
    {
        if name == "__ku_object"
            && args.chunks_exact(2).any(|field| match &field[0].kind {
                IrExprKind::Literal(key) if key == "\"tls\"" => {
                    !matches!(&field[1].kind, IrExprKind::Literal(value) if value == "false")
                }
                IrExprKind::Literal(key)
                    if key == "\"tls_server_name\"" || key == "\"tls_ca_pem\"" =>
                {
                    true
                }
                _ => false,
            })
        {
            return true;
        }
    }
    expr_children(expr)
        .into_iter()
        .any(expr_mentions_native_tls_config)
}

fn expr_uses_intrinsic(expr: &IrExpr, intrinsic: &str) -> bool {
    matches!(
        &expr.kind,
        IrExprKind::Call {
            kind: IrCallKind::Intrinsic(name),
            ..
        } if name == intrinsic
    ) || expr_children(expr)
        .into_iter()
        .any(|child| expr_uses_intrinsic(child, intrinsic))
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

fn collect_result_inners_program(program: &IrProgram, output: &mut Vec<IrType>) -> KuResult<()> {
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
                // Result-bearing timeout returns are collected from the explicit
                // timeout target block, not from this edge-only terminator.
                IrTerminator::Safepoint { .. } => {}
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
                | IrType::Float
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

fn emit_main_wrapper(
    out: &mut COutput,
    program: &IrProgram,
    fs_usage: FsUsage,
    fs_base: &NativeFsBase,
) -> KuResult<()> {
    out.check()?;
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
    if fs_usage.any() && matches!(fs_base, NativeFsBase::ExecutableRelative(_)) {
        // Initialization is deliberately non-fatal. Absolute paths remain usable
        // even if the executable-relative source locator cannot be resolved.
        out.push_str("  (void)ku_fs_init_base();\n");
    }
    let mysql_shutdown = if program_uses_mysql(program) {
        "  ku_mysql_thread_shutdown();\n"
    } else {
        ""
    };
    match &function.return_type {
        IrType::Void => {
            out.push_str("  ku_main();\n");
            out.push_str(mysql_shutdown);
            out.push_str("  return 0;\n");
        }
        IrType::Int => {
            out.push_str("  int exit_code = (int)ku_main();\n");
            out.push_str(mysql_shutdown);
            out.push_str("  return exit_code;\n");
        }
        IrType::Bool => {
            out.push_str("  int exit_code = ku_main() ? 0 : 1;\n");
            out.push_str(mysql_shutdown);
            out.push_str("  return exit_code;\n");
        }
        IrType::Str => {
            out.push_str("  KuString result = ku_main();\n  ku_string_write(stdout, result);\n  fputc('\\n', stdout);\n  ku_string_drop(&result);\n");
            out.push_str(mysql_shutdown);
            out.push_str("  return 0;\n");
        }
        IrType::Result(inner) => {
            out.push_str(&format!(
                "  {} result = ku_main();\n  if (!result.ok) {{ ku_string_write(stderr, result.error.message); fputc('\\n', stderr); ku_result_drop_{}(&result);{}  return 1; }}\n  ku_result_drop_{}(&result);\n{}  return 0;\n",
                c_type(&function.return_type)?,
                c_type_suffix(inner)?,
                mysql_shutdown,
                c_type_suffix(inner)?,
                mysql_shutdown
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

/// A static-storage `KuString` whose generated C spelling is ASCII-only. This
/// is used for the CLI-computed fs locator, which may itself contain Unicode.
fn c_static_utf8_string(text: &str) -> String {
    let mut literal = String::from("\"");
    for &byte in text.as_bytes() {
        match byte {
            b'"' => literal.push_str("\\\""),
            b'\\' => literal.push_str("\\\\"),
            b'\n' => literal.push_str("\\n"),
            b'\r' => literal.push_str("\\r"),
            b'\t' => literal.push_str("\\t"),
            0x20..=0x7e => literal.push(byte as char),
            _ => literal.push_str(&format!("\\{byte:03o}")),
        }
    }
    literal.push('"');
    format!(
        "ku_string_static((const uint8_t*){literal}, {})",
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
            _ => {
                return Err(unsupported(format!(
                    "native C {name} expects 1 or 2 arguments"
                )))
            }
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
            _ => {
                return Err(unsupported(
                    "native C http.redirect expects 1 or 2 arguments",
                ))
            }
        };
        return Ok(Some(format!(
            "({resp_ty}){{ {status}, (KuString){{0}}, (KuString){{0}}, {location} }}"
        )));
    }
    // app.listen(address): run the admission-controlled accept/worker loop.
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
        // Borrow the closure value; ku_http_server_add_route retains its own env
        // reference, and the route table releases that reference on server free.
        let handler = c_expr(handler)?;
        return Ok(Some(format!(
            "(ku_http_server_add_route({}, {}, {}, (void*)({handler}).invoke, ({handler}).env, {arity}, {returns_result}), (uint8_t)0)",
            c_expr(server)?,
            c_static_string(method),
            c_value_expr(path)?
        )));
    }
    Ok(None)
}

/// Lower the public `pg.client(config)` constructor and private receiver
/// intrinsics. Reads borrow their handles; `client.close()` moves and clears the
/// client so scope cleanup cannot close it twice.
fn c_pg_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("pg.{method} missing argument")))
    };
    match method {
        "client" => Ok(format!("ku_pg_client({})", c_expr(arg(0)?)?)),
        "query" => Ok(format!(
            "ku_pg_client_query({}, {}, {})",
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
        "is_null" => Ok(format!(
            "ku_pg_is_null({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "close" => Ok(format!("ku_pg_client_close({})", c_value_expr(arg(0)?)?)),
        other => Err(unsupported(format!(
            "native C pg.{other}() is not implemented"
        ))),
    }
}

/// Lower the unique MySQL client constructor and receiver methods. Query and
/// execute borrow the synchronized pool; close consumes it.
fn c_mysql_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("mysql.{method} missing argument")))
    };
    match method {
        "client" => Ok(format!("ku_mysql_client_new({})", c_expr(arg(0)?)?)),
        "query" => Ok(format!(
            "ku_mysql_client_query({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "execute" => Ok(format!(
            "ku_mysql_client_execute({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "rows" => Ok(format!("ku_mysql_result_rows({})", c_expr(arg(0)?)?)),
        "cols" => Ok(format!("ku_mysql_result_cols({})", c_expr(arg(0)?)?)),
        "value" => Ok(format!(
            "ku_mysql_result_value({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "is_null" => Ok(format!(
            "ku_mysql_result_is_null({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "close" => Ok(format!("ku_mysql_client_close({})", c_value_expr(arg(0)?)?)),
        other => Err(unsupported(format!(
            "native C mysql.{other}() is not implemented"
        ))),
    }
}

/// Lower the unique Redis client constructor and its receiver-method intrinsics.
/// Commands borrow the bounded pool; `client.close()` consumes it.
fn c_redis_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("redis.{method} missing argument")))
    };
    match method {
        "client" => Ok(format!("ku_redis_client({})", c_expr(arg(0)?)?)),
        "ping" => Ok(format!("ku_redis_ping({})", c_expr(arg(0)?)?)),
        "get" => Ok(format!(
            "ku_redis_get({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "set" => Ok(format!(
            "ku_redis_set({}, {}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?,
            c_expr(arg(2)?)?
        )),
        "del" => Ok(format!(
            "ku_redis_del({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "exists" => Ok(format!(
            "ku_redis_exists({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "close" => Ok(format!("ku_redis_close({})", c_value_expr(arg(0)?)?)),
        other => Err(unsupported(format!(
            "native C redis.{other}() is not implemented"
        ))),
    }
}

fn c_bytes_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("bytes.{method} missing argument")))
    };
    match method {
        "from_str" => Ok(format!("ku_bytes_from_str({})", c_expr(arg(0)?)?)),
        "from_array" => Ok(format!("ku_bytes_from_array({})", c_expr(arg(0)?)?)),
        "len" => Ok(format!("ku_bytes_len({})", c_expr(arg(0)?)?)),
        "get" => Ok(format!(
            "ku_bytes_get({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "to_str" => Ok(format!("ku_bytes_to_str({})", c_expr(arg(0)?)?)),
        other => Err(unsupported(format!(
            "native C bytes.{other}() is not implemented"
        ))),
    }
}

fn c_net_intrinsic_expr(method: &str, args: &[IrExpr]) -> KuResult<String> {
    let arg = |i: usize| -> KuResult<&IrExpr> {
        args.get(i)
            .ok_or_else(|| unsupported(format!("net.{method} missing argument")))
    };
    match method {
        "client" => Ok(format!("ku_net_client({})", c_expr(arg(0)?)?)),
        "read" => Ok(format!(
            "ku_net_read({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "write" => Ok(format!(
            "ku_net_write({}, {})",
            c_expr(arg(0)?)?,
            c_expr(arg(1)?)?
        )),
        "close" => Ok(format!("ku_net_close({})", c_value_expr(arg(0)?)?)),
        other => Err(unsupported(format!(
            "native C net.{other}() is not implemented"
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
        // Byte offsets need no scan or allocation; KuString already stores them.
        "byte_len" => Ok(format!("(int64_t)({}).len", c_expr(recv)?)),
        "chars" => Ok(format!("ku_string_chars({})", c_expr(recv)?)),
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
    if matches!(name, "__ku_array_push_reuse" | "__ku_string_concat_reuse") {
        let [receiver, value] = args else {
            return Err(unsupported(
                "native collection reuse requires a local and value",
            ));
        };
        // These internal operations are emitted only for an exact local or cell
        // self-assignment. Take the address of its payload without moving it in
        // a C argument: C does not specify argument evaluation order. The helper
        // clears the receiver only after the RHS and its new payload are ready.
        let exact_binding_place = matches!(&receiver.kind, IrExprKind::Local(_))
            || matches!(
                &receiver.kind,
                IrExprKind::CellLoad(cell)
                    if matches!(cell.kind, IrExprKind::Local(_) | IrExprKind::CapturedCell(_))
            );
        if !exact_binding_place {
            return Err(unsupported(
                "native collection reuse requires an exact local or cell binding",
            ));
        }
        let receiver_place = c_move_place(receiver)?.ok_or_else(|| {
            unsupported("native collection reuse requires a local or cell payload")
        })?;
        let value_expr = c_expr(value)?;
        return match (&receiver.ty, name) {
            (IrType::Array(element), "__ku_array_push_reuse") => Ok(format!(
                "ku_array_push_reuse_{}(&{}, {})",
                c_type_suffix(element)?,
                receiver_place,
                value_expr
            )),
            (IrType::Str, "__ku_string_concat_reuse") if value.ty == IrType::Str => Ok(format!(
                "ku_string_concat_reuse(&{receiver_place}, {value_expr})"
            )),
            _ => Err(unsupported(
                "native collection reuse has incompatible types",
            )),
        };
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
                return Ok(format!(
                    "ku_array_is_empty_{}({})",
                    suffix,
                    c_expr(receiver)?
                ))
            }
            "push" => {
                let value = args
                    .get(1)
                    .ok_or_else(|| unsupported("native C array.push requires a value argument"))?;
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
                let mapper = args
                    .get(1)
                    .ok_or_else(|| unsupported("native C array.map requires a mapper closure"))?;
                let IrType::Closure {
                    params,
                    param_modes,
                    ret,
                } = &mapper.ty
                else {
                    return Err(unsupported(
                        "native C array.map requires a closure argument",
                    ));
                };
                let cl_suffix = closure_signature_suffix(params, param_modes, ret)?;
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
    if name == "time.now" {
        if !args.is_empty() {
            return Err(unsupported("native C time.now expects no arguments"));
        }
        return Ok("ku_time_now_millis()".to_string());
    }
    if name == "time.instant" {
        if !args.is_empty() {
            return Err(unsupported("native C time.instant expects no arguments"));
        }
        return Ok("ku_time_instant()".to_string());
    }
    if name == "time.elapsed" {
        return match args {
            [previous] if matches!(&previous.ty, IrType::Named(type_name) if type_name == "__ku_time") => {
                Ok(format!("ku_time_elapsed({})", c_expr(previous)?))
            }
            [_] => Err(unsupported(
                "native C time.elapsed(value) requires a value returned by time.instant()",
            )),
            _ => Err(unsupported("native C time.elapsed expects one argument")),
        };
    }
    if name == "time.millis" {
        return match args {
            [] => Ok("ku_time_now_millis()".to_string()),
            [value] if matches!(&value.ty, IrType::Named(type_name) if type_name == "__ku_time") => {
                Ok(format!("ku_time_value_millis({})", c_expr(value)?))
            }
            [_] => Err(unsupported(
                "native C time.millis(value) currently requires a value returned by time.instant()",
            )),
            _ => Err(unsupported(
                "native C time.millis expects zero or one argument",
            )),
        };
    }
    if name == "time.steady_millis" {
        if !args.is_empty() {
            return Err(unsupported(
                "native C time.steady_millis expects no arguments",
            ));
        }
        return Ok("ku_time_steady_millis()".to_string());
    }
    if name == "fs.read" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.read requires a path"))?;
        return Ok(format!("ku_fs_read({})", c_expr(path)?));
    }
    if name == "fs.try_read" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.try_read requires a path"))?;
        return Ok(format!("ku_fs_try_read({})", c_expr(path)?));
    }
    if name == "fs.write" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.write requires a path"))?;
        let content = args
            .get(1)
            .ok_or_else(|| unsupported("native C fs.write requires content"))?;
        return Ok(format!(
            "ku_fs_write({}, {})",
            c_expr(path)?,
            c_expr(content)?
        ));
    }
    if name == "fs.try_write" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.try_write requires a path"))?;
        let content = args
            .get(1)
            .ok_or_else(|| unsupported("native C fs.try_write requires content"))?;
        return Ok(format!(
            "ku_fs_try_write({}, {})",
            c_expr(path)?,
            c_expr(content)?
        ));
    }
    if name == "fs.exists" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.exists requires a path"))?;
        return Ok(format!("ku_fs_exists({})", c_expr(path)?));
    }
    if name == "fs.read_dir" {
        let path = args
            .first()
            .ok_or_else(|| unsupported("native C fs.read_dir requires a path"))?;
        return Ok(format!("ku_fs_read_dir({})", c_expr(path)?));
    }
    if name == "json.stringify" {
        let value = args
            .first()
            .ok_or_else(|| unsupported("native C json.stringify requires a value"))?;
        if json_typed_root_required(&value.ty) {
            return Ok(format!(
                "ku_json_stringify_typed_{}({})",
                c_type_suffix(&value.ty)?,
                c_expr(value)?
            ));
        }
        // A KuValue is already boxed; other types get wrapped into one.
        let arg = if matches!(&value.ty, IrType::Named(n) if n == "__ku_value") {
            c_expr(value)?
        } else {
            ku_value_wrap(&value.ty, &c_expr(value)?)?
        };
        return Ok(format!("ku_json_stringify({})", arg));
    }
    if name == "json.parse" {
        let text = args
            .first()
            .ok_or_else(|| unsupported("native C json.parse requires a string"))?;
        return Ok(format!("ku_json_parse({})", c_expr(text)?));
    }
    if name == "json.try_parse" {
        let text = args
            .first()
            .ok_or_else(|| unsupported("native C json.try_parse requires a string"))?;
        return Ok(format!("ku_json_try_parse({})", c_expr(text)?));
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
    if name == "pg.client" {
        return c_pg_intrinsic_expr("client", args);
    }
    if name.starts_with("pg.") {
        return Err(unsupported(
            "native C std.pg exposes only pg.client(config); use receiver methods on the client and result",
        ));
    }
    if let Some(method) = name.strip_prefix("pg_client.") {
        return c_pg_intrinsic_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("pg_result.") {
        return c_pg_intrinsic_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("redis.") {
        return c_redis_intrinsic_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("bytes.") {
        return c_bytes_intrinsic_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("net.") {
        return c_net_intrinsic_expr(method, args);
    }
    if let Some(method) = name.strip_prefix("mysql.") {
        return c_mysql_intrinsic_expr(method, args);
    }
    if name == "len" {
        let [value] = args else {
            return Err(unsupported("native C len expects one argument"));
        };
        return match &value.ty {
            IrType::Str => c_string_method_expr("len", args),
            IrType::Array(element) => Ok(format!(
                "ku_array_len_{}({})",
                c_type_suffix(element)?,
                c_expr(value)?
            )),
            _ => Err(unsupported("native C len requires a string or array")),
        };
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
        (name @ ("err" | "fail"), IrType::Result(inner)) => {
            let value = args
                .first()
                .ok_or_else(|| unsupported(format!("{name} requires one argument")))?;
            Ok(format!(
                "({}){{ false, {}, {} }}",
                c_type(ty)?,
                c_zero_value(inner)?,
                c_error_expr(value, name)?
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
        IrType::Float => Ok("0.0".to_string()),
        IrType::Bool => Ok("false".to_string()),
        IrType::Str => Ok("(KuString){0}".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok("NULL".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("ku_v_null()".to_string()),
        IrType::Named(name) if name == "__ku_time" => Ok("(KuTime){0}".to_string()),
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
        IrType::Float => Ok("0.0".to_string()),
        IrType::Bool => Ok("false".to_string()),
        IrType::Str => Ok("(KuString){0}".to_string()),
        IrType::Named(name) if name == "__ku_object" => Ok("NULL".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("ku_v_null()".to_string()),
        IrType::Named(name) if name == "__ku_time" => Ok("(KuTime){0}".to_string()),
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
        IrType::Array(element) => Ok(format!(
            "ku_array_move_{}(&{place})",
            c_type_suffix(element)?
        )),
        IrType::Result(inner) => Ok(format!(
            "ku_result_move_{}(&{place})",
            c_type_suffix(inner)?
        )),
        IrType::Named(name) if name == "__ku_object" => Ok(format!("ku_object_move(&{place})")),
        IrType::Named(name) if name == "__ku_value" => Ok(format!("ku_value_move(&{place})")),
        IrType::Named(name) if name == "__ku_time" => Ok(place.to_string()),
        IrType::Named(name) if name == "__ku_http_server" => Ok(place.to_string()),
        IrType::Named(name) if name == "__ku_error_type" => Ok(format!("ku_error_move(&{place})")),
        IrType::Named(name) => Ok(format!("{}(&{place})", c_named_move_function(name))),
        IrType::Closure {
            params,
            param_modes,
            ret,
        } => Ok(format!(
            "ku_closure_move_{}(&{place})",
            closure_signature_suffix(params, param_modes, ret)?
        )),
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null => Ok(place.to_string()),
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
        IrType::Result(inner) => Ok(format!(
            "ku_result_clone_{}({expression})",
            c_type_suffix(inner)?
        )),
        IrType::Named(name) if name == "__ku_error_type" => {
            Ok(format!("ku_error_clone({expression})"))
        }
        IrType::Named(name) if name == "__ku_object" => {
            Ok(format!("ku_object_clone({expression})"))
        }
        IrType::Named(name) if name == "__ku_value" => Ok(format!("ku_value_clone({expression})")),
        IrType::Named(name) if name == "__ku_time" => Ok(expression.to_string()),
        // Stage 8a: the server value is a shared heap pointer, cloned by copy.
        IrType::Named(name) if name == "__ku_http_server" => Ok(expression.to_string()),
        IrType::Named(name) => Ok(format!("{}({expression})", c_named_clone_function(name))),
        IrType::Str => Ok(format!("ku_string_clone({expression})")),
        // Stage 6e: cloning a stored closure shares its captured environment by
        // bumping the env refcount (env==NULL for a Stage 6a no-capture closure
        // makes this a plain struct copy).
        IrType::Closure {
            params,
            param_modes,
            ret,
        } => Ok(format!(
            "ku_closure_clone_{}({expression})",
            closure_signature_suffix(params, param_modes, ret)?
        )),
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null => Ok(expression.to_string()),
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
        IrType::Result(inner) => Ok(format!(
            "ku_result_drop_{}(&{expression});",
            c_type_suffix(inner)?
        )),
        IrType::Named(name) if name == "__ku_error_type" => {
            Ok(format!("ku_error_drop(&{expression});"))
        }
        IrType::Named(name) if name == "__ku_object" => {
            Ok(format!("ku_object_drop({expression});"))
        }
        IrType::Named(name) if name == "__ku_value" => {
            Ok(format!("ku_value_drop(&{expression});"))
        }
        IrType::Named(name) if name == "__ku_time" => Ok(String::new()),
        // Stage 8a: the server outlives every local (freed by the accept loop on
        // exit, or reclaimed by the OS), so a local going out of scope drops nothing.
        IrType::Named(name) if name == "__ku_http_server" => Ok(String::new()),
        IrType::Named(name) => {
            Ok(format!("{}(&{expression});", c_named_drop_function(name)))
        }
        IrType::Str => Ok(format!("ku_string_drop(&{expression});")),
        // Stage 6e: a stored closure owns a reference to its captured env; drop
        // releases it (env==NULL for a Stage 6a no-capture closure is a no-op).
        IrType::Closure { .. } => Ok(format!(
            "if (({expression}).env) ((KuEnvHeader*)({expression}).env)->release(({expression}).env);"
        )),
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null => Ok(String::new()),
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
        IrType::Float => Ok("float".to_string()),
        IrType::Bool => Ok("bool".to_string()),
        IrType::Str => Ok("str".to_string()),
        IrType::Null => Ok("null".to_string()),
        IrType::Named(name) if name == "__ku_value" => Ok("kuvalue".to_string()),
        IrType::Named(name) if name == "__ku_time" => Ok("time".to_string()),
        IrType::Named(name) if name == "__ku_http_server" => Ok("http_server".to_string()),
        IrType::Named(name) if name == "__ku_bytes" => Ok("bytes".to_string()),
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
        IrType::Closure {
            params,
            param_modes,
            ret,
        } => closure_signature_suffix(params, param_modes, ret),
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
    match ty {
        IrType::Str | IrType::Array(_) | IrType::Result(_) | IrType::Closure { .. } => true,
        IrType::Named(name) => name != "__ku_time",
        _ => false,
    }
}

fn collect_owned_locals(
    function: &IrFunction,
    for_each_states: &[ForEachState],
) -> Vec<OwnedLocal> {
    let mut locals = Vec::new();
    for param in &function.params {
        if param.mode == ParamMode::Owned && is_c_owned_type(&param.ty) {
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
                    borrowed: crate::ir::ir_expr_is_borrowed(value)
                        || matches!(
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
    for state in for_each_states {
        if let IrType::Array(_) = &state.iterable_ty {
            let prefix = for_state_prefix(state.block_id);
            locals.push(OwnedLocal {
                source_name: format!("{prefix}_array"),
                name: format!("{prefix}_array"),
                ty: state.iterable_ty.clone(),
                is_param: false,
                borrowed: false,
            });
        }
        if is_c_owned_type(&state.element_ty) {
            locals.push(OwnedLocal {
                source_name: state.name.clone(),
                name: c_ident(&state.name),
                ty: state.element_ty.clone(),
                is_param: false,
                borrowed: false,
            });
        }
    }
    locals
}

fn emit_owned_cleanup(out: &mut COutput, locals: &[OwnedLocal]) -> KuResult<()> {
    out.check()?;
    for local in locals.iter().rev() {
        if local.borrowed {
            continue;
        }
        emit_drop_expr(out, &local.ty, &local.name)?;
    }
    Ok(())
}

fn emit_drop_expr(out: &mut COutput, ty: &IrType, expression: &str) -> KuResult<()> {
    out.check()?;
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
        IrType::Named(name) if name == "__ku_time" => Ok(()),
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
    matches!(
        ty,
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null
    )
}

fn emit_named_ownership_helpers(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    let has_any = !program.layouts.structs.is_empty() || !program.layouts.enums.is_empty();
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

fn emit_named_ownership_prototypes(out: &mut COutput, name: &str) -> KuResult<()> {
    out.check()?;
    let c_ty = c_type(&IrType::Named(name.to_string()))?;
    out.push_str(&format!(
        "static {c_ty} {}({c_ty}* value);\nstatic {c_ty} {}({c_ty} value);\nstatic void {}({c_ty}* value);\n",
        c_named_move_function(name),
        c_named_clone_function(name),
        c_named_drop_function(name),
    ));
    Ok(())
}

fn emit_struct_ownership_helper(out: &mut COutput, layout: &IrStructLayout) -> KuResult<()> {
    out.check()?;
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

fn emit_enum_ownership_helper(out: &mut COutput, layout: &IrEnumLayout) -> KuResult<()> {
    out.check()?;
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
fn emit_named_ownership_helper(out: &mut COutput, name: &str, is_enum: bool) -> KuResult<()> {
    out.check()?;
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
/// routes generic Named ownership dispatch to the dedicated native helpers.
fn pg_native_suffix(name: &str) -> Option<&'static str> {
    match name {
        "__ku_pg_result" => Some("pg_result"),
        "__ku_pg_client" => Some("pg_client"),
        "__ku_redis_client" => Some("redis_client"),
        "__ku_net_client" => Some("net_client"),
        "__ku_mysql_client" => Some("mysql_client"),
        "__ku_mysql_result" => Some("mysql_result"),
        _ => None,
    }
}

fn c_named_suffix(name: &str) -> String {
    if name == "__ku_bytes" {
        return "bytes".to_string();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::TempId;

    #[test]
    fn native_c_output_whole_file_limit_counts_runtime_and_generated_code() {
        for source in [
            "fn main() { println(\"界\") }",
            "fn Identity<T>(value: T): T { return value } fn main() { println(Identity(7)) }",
        ] {
            let program =
                crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                    .parse_program()
                    .unwrap();
            let ir = crate::ir::lower_program(&program).unwrap();
            let expected = generate_c_source(&ir).unwrap();
            let options = CBackendOptions::default();
            assert_eq!(
                generate_c_source_bounded(&ir, &options, expected.len()).unwrap(),
                expected,
            );
            let error = generate_c_source_bounded(&ir, &options, expected.len() - 1).unwrap_err();
            assert!(
                error.message.contains("native C output limit exceeded"),
                "{error}"
            );
            let error = generate_c_source_bounded(&ir, &options, 1).unwrap_err();
            assert!(error.message.contains("maximum 1 bytes"), "{error}");
            assert_eq!(generate_c_source(&ir).unwrap(), expected);
        }
    }

    #[test]
    fn native_c_output_stops_before_emitting_next_function_after_failure() {
        let mut output = COutput::new(0);
        output.push('x');
        let function = IrFunction {
            id: FunctionId(0),
            name: "unvisited".into(),
            params: Vec::new(),
            // Without the early checkpoint this would produce an unrelated
            // unsupported-type error, hiding the original output limit.
            return_type: IrType::Unknown,
            blocks: Vec::new(),
            is_closure_body: false,
            captures: Vec::new(),
        };
        let error = emit_function(&mut output, &function).unwrap_err();
        assert!(
            error.message.contains("native C output limit exceeded"),
            "{error}"
        );
    }

    #[test]
    fn collection_reuse_rejects_non_binding_places() {
        let array_type = IrType::Array(Box::new(IrType::Int));
        let receiver = IrExpr {
            kind: IrExprKind::Temp(TempId(7)),
            ty: array_type.clone(),
        };
        let value = IrExpr {
            kind: IrExprKind::Literal("1".to_string()),
            ty: IrType::Int,
        };

        let error = c_intrinsic_expr("__ku_array_push_reuse", &[receiver, value], &array_type)
            .expect_err("a temp must never become a collection reuse receiver");
        assert!(
            error
                .to_string()
                .contains("requires an exact local or cell binding"),
            "unexpected rejection: {error}"
        );
    }
}

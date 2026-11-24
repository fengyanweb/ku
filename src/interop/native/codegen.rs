use super::{
    err,
    signatures::{BridgeType, Export, Integer, Operation},
    KuResult, Language, NativeDescriptor,
};

const SUPPORT: &str = r#"
#define KU_NATIVE_TEXT_LIMIT ((size_t)1000000)

static const uint8_t ku_native_copy_failed[] = "native bridge text allocation failed";

static int ku_native_text(KuInteropValue* out, const char* source) {
    if (!source) return 0;
    size_t count = 0;
    while (count <= KU_NATIVE_TEXT_LIMIT && source[count]) ++count;
    if (count > KU_NATIVE_TEXT_LIMIT) return 0;
    uint8_t* data = count ? (uint8_t*)malloc(count) : NULL;
    if (count && !data) return 0;
    if (count) memcpy(data, source, count);
    KuInteropValue result = { KU_INTEROP_STR, 0, 0, data, count };
    *out = result;
    return 1;
}

static int ku_native_fail(KuInteropValue* out, const char* message) {
    if (!ku_native_text(out, message)) {
        KuInteropValue failure = {
            KU_INTEROP_STR, 0, 0, ku_native_copy_failed, sizeof(ku_native_copy_failed) - 1
        };
        *out = failure;
    }
    return 1;
}

static void ku_native_integer(KuInteropValue* out, uint32_t tag, int64_t integer) {
    KuInteropValue result = { tag, 0, integer, NULL, 0 };
    *out = result;
}

KU_INTEROP_EXPORT void ku_interop_free_v1(KuInteropValue* out) {
    if (out->tag == KU_INTEROP_STR && out->data != ku_native_copy_failed) {
#ifdef KU_NATIVE_TRANSFERS_OWNED_TEXT
        if (!ku_native_release_owned(out->data))
#endif
            free((void*)out->data);
    }
    KuInteropValue empty = { KU_INTEROP_NULL, 0, 0, NULL, 0 };
    *out = empty;
}

static inline int ku_native_cstr_valid(KuInteropValue input) {
    return input.length <= KU_NATIVE_TEXT_LIMIT &&
        (input.length == 0 || (input.data && !memchr(input.data, 0, input.length)));
}
"#;

const C_STRINGS: &str = r#"
static char* ku_native_make_cstr(KuInteropValue input) {
    char* content = (char*)malloc(input.length + 1);
    if (!content) return NULL;
    if (input.length) memcpy(content, input.data, input.length);
    content[input.length] = '\0';
    return content;
}

static void ku_native_release_cstr(char** slots, size_t count) {
    for (size_t i = 0; i < count; ++i) free(slots[i]);
}
"#;

const C_OWNED_TEXT: &str = r#"
#if defined(_MSC_VER)
#define KU_NATIVE_THREAD_LOCAL __declspec(thread)
#else
#define KU_NATIVE_THREAD_LOCAL _Thread_local
#endif

#define KU_NATIVE_TRANSFERS_OWNED_TEXT 1
typedef void (*KuNativeTextRelease)(char*);

/* One initialized owned output per caller thread. The host copies and frees
 * every returned value on that same thread before issuing another ABI call. */
static KU_NATIVE_THREAD_LOCAL char* ku_native_owned_text;
static KU_NATIVE_THREAD_LOCAL KuNativeTextRelease ku_native_owned_release;

static int ku_native_release_owned(const uint8_t* candidate) {
    if (!ku_native_owned_text || (const uint8_t*)ku_native_owned_text != candidate)
        return 0;
    char* memory = ku_native_owned_text;
    KuNativeTextRelease release = ku_native_owned_release;
    ku_native_owned_text = NULL;
    ku_native_owned_release = NULL;
    release(memory);
    return 1;
}

static int ku_native_offer_owned(
    KuInteropValue* out, char* text, KuNativeTextRelease release, size_t limit) {
    if (!text || ku_native_owned_text) return 0;
    size_t count = 0;
    while (count <= limit && text[count]) ++count;
    if (count > limit) return 0;
    ku_native_owned_text = text;
    ku_native_owned_release = release;
    KuInteropValue value = { KU_INTEROP_STR, 0, 0, (const uint8_t*)text, count };
    *out = value;
    return 1;
}
"#;

const CPP_OWNERS: &str = r#"
static_assert(std::is_nothrow_destructible<KuNativeOwner>::value,
              "a Ku interop owner must have a noexcept destructor");

struct KuNativeSlot {
    int64_t id;
    std::unique_ptr<KuNativeOwner> owner;
};

static std::mutex ku_native_owner_lock;
static KuNativeSlot ku_native_owners[64];
static int64_t ku_native_next_owner;

static KuNativeSlot* ku_native_find(int64_t id) {
    if (id <= 0) return nullptr;
    for (KuNativeSlot& slot : ku_native_owners) {
        if (slot.id == id) return &slot;
    }
    return nullptr;
}

static int ku_native_new(KuInteropValue* out) {
    if (ku_native_next_owner == INT64_MAX)
        return ku_native_fail(out, "native owner identity exhausted");
    for (KuNativeSlot& slot : ku_native_owners) {
        if (!slot.owner) {
            std::unique_ptr<KuNativeOwner> owner = std::make_unique<KuNativeOwner>();
            slot.id = ++ku_native_next_owner;
            slot.owner = std::move(owner);
            ku_native_integer(out, KU_INTEROP_INT, slot.id);
            return 0;
        }
    }
    return ku_native_fail(out, "native owner limit reached");
}

template <class T>
static int ku_native_checked_integer(KuInteropValue* out, T value) {
    if constexpr (std::is_enum_v<T>) {
        return ku_native_checked_integer(out, static_cast<std::underlying_type_t<T>>(value));
    } else {
        static_assert(std::is_integral_v<T> && sizeof(T) <= sizeof(int64_t),
                      "declared integer method requires an integer or enum C++ return type");
        if constexpr (std::is_unsigned_v<T>) {
            if ((uint64_t)value > (uint64_t)INT64_MAX)
                return ku_native_fail(out, "native integer result exceeds Ku int range");
        }
        ku_native_integer(out, KU_INTEROP_INT, (int64_t)value);
        return 0;
    }
}
"#;

pub(super) fn producer_source(
    descriptor: &NativeDescriptor,
    exports: &[Export],
) -> KuResult<String> {
    let header = descriptor
        .header
        .to_str()
        .ok_or_else(|| err("native_descriptor", "native header path is not UTF-8"))?
        .replace('\\', "/");
    let mut source = format!(
        "#include \"ku_interop_abi.h\"\n#include <limits.h>\n#include <stdlib.h>\n#include <string.h>\n#include \"{header}\"\n"
    );
    let cpp = descriptor.language == Language::Cpp;
    if cpp {
        source.push_str(
            "#include <exception>\n#include <memory>\n#include <mutex>\n#include <string>\n#include <type_traits>\n",
        );
    }
    if exports.iter().any(|export| {
        matches!(
            export.operation,
            Operation::C {
                release: Some(_),
                ..
            }
        )
    }) {
        source.push_str(C_OWNED_TEXT);
        for export in exports {
            if let Operation::C {
                release: Some(release),
                ..
            } = &export.operation
            {
                source.push_str(&format!(
                    "static void ku_native_drop_{}(char* value) {{ {release}(value); }}\n",
                    export.name
                ));
            }
        }
    }
    source.push_str(SUPPORT);
    if cpp {
        let class = descriptor
            .class
            .as_deref()
            .expect("validated C++ descriptor");
        source.push_str(&format!("using KuNativeOwner = {class};\n"));
        source.push_str(CPP_OWNERS);
    } else if exports.iter().any(|export| {
        export
            .args
            .iter()
            .any(|arg| matches!(arg, BridgeType::Text))
    }) {
        source.push_str(C_STRINGS);
    }
    source.push_str(
        r#"
KU_INTEROP_EXPORT int ku_interop_call_v1(
    const char* operation, const KuInteropValue* args, size_t nargs, KuInteropValue* out) {
    if (!out) return 1;
    if (!operation || (nargs && !args)) return ku_native_fail(out, "missing bridge argument array");
"#,
    );
    if cpp {
        source.push_str("    try {\n        const std::lock_guard<std::mutex> owner_guard(ku_native_owner_lock);\n");
    }
    for export in exports {
        emit_operation(&mut source, export, cpp)?;
    }
    source.push_str("    return ku_native_fail(out, \"unknown interop operation\");\n");
    if cpp {
        source.push_str(
            r#"    } catch (const std::exception& error) {
        return ku_native_fail(out, error.what());
    } catch (...) {
        return ku_native_fail(out, "C++ exception at the Ku interop boundary");
    }
"#,
        );
    }
    source.push_str("}\n");
    Ok(source)
}

fn emit_operation(source: &mut String, export: &Export, cpp: bool) -> KuResult<()> {
    let name = &export.name;
    let len = export.args.len();
    source.push_str(&format!(
        "    if (strcmp(operation, \"{name}\") == 0) {{\n        if (nargs != {len}) return ku_native_fail(out, \"unexpected native argument count\");\n"
    ));
    // Validate all arguments before allocating text or running a foreign method.
    for (index, arg) in export.args.iter().enumerate() {
        source.push_str(&format!(
            "        if (args[{index}].tag != {} || args[{index}].reserved != 0) return ku_native_fail(out, \"unexpected native argument type\");\n",
            arg.tag()
        ));
        match arg {
            BridgeType::Bool => source.push_str(&format!(
                "        if (args[{index}].integer != 0 && args[{index}].integer != 1) return ku_native_fail(out, \"invalid native bool argument\");\n"
            )),
            BridgeType::Int(integer) => {
                emit_integer_guard(source, index, integer);
            }
            BridgeType::Text => source.push_str(&format!(
                "        if (!ku_native_cstr_valid(args[{index}])) return ku_native_fail(out, \"native C strings cannot contain NUL or exceed the text limit\");\n"
            )),
            BridgeType::Buffer { length, .. } => source.push_str(&format!(
                "        if (args[{length}].integer < 0 || (uint64_t)args[{length}].integer > args[{index}].length || (args[{index}].length && !args[{index}].data)) return ku_native_fail(out, \"native buffer length is outside its borrowed input\");\n"
            )),
            BridgeType::Void => return Err(err("native_signature", "void cannot appear in a bridge argument")),
        }
    }
    let c_owned_strings = !cpp
        && export
            .args
            .iter()
            .any(|arg| matches!(arg, BridgeType::Text));
    if c_owned_strings {
        source.push_str(&format!("        char* owned_text[{len}] = {{0}};\n"));
        for (index, arg) in export.args.iter().enumerate() {
            if let BridgeType::Text = arg {
                source.push_str(&format!(
                    "        owned_text[{index}] = ku_native_make_cstr(args[{index}]);\n\
                     \x20       if (!owned_text[{index}]) {{ ku_native_release_cstr(owned_text, {len}); return ku_native_fail(out, \"could not allocate native C string argument\"); }}\n"
                ));
            }
        }
    }
    if cpp {
        for (index, arg) in export.args.iter().enumerate() {
            if let BridgeType::Text = arg {
                source.push_str(&format!(
                    "        std::string owned_text_{index}(args[{index}].length ? reinterpret_cast<const char*>(args[{index}].data) : \"\", args[{index}].length);\n"
                ));
            }
        }
    }
    let foreign_args = |first| {
        export
            .args
            .iter()
            .enumerate()
            .skip(first)
            .map(|(index, arg)| match arg {
                BridgeType::Bool => format!("(args[{index}].integer != 0)"),
                BridgeType::Int(int) => format!("({})args[{index}].integer", int.spelling),
                BridgeType::Text if cpp => format!("owned_text_{index}.c_str()"),
                BridgeType::Text => format!("owned_text[{index}]"),
                BridgeType::Buffer { length, spelling } => {
                    format!("({spelling})(args[{length}].integer ? args[{index}].data : NULL)")
                }
                BridgeType::Void => unreachable!("validated argument"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    match &export.operation {
        Operation::C { symbol, release } => {
            let call = format!("{symbol}({})", foreign_args(0));
            let cleanup = if c_owned_strings {
                format!("        ku_native_release_cstr(owned_text, {len});\n")
            } else {
                String::new()
            };
            emit_c_result(
                source, call, &export.result, release.as_ref().map(|_| export.name.as_str()), &cleanup,
            );
        }
        Operation::Create => source.push_str("        return ku_native_new(out);\n"),
        Operation::Destroy => source.push_str(
            "        KuNativeSlot* slot = ku_native_find(args[0].integer);\n\
             \x20       if (!slot) return ku_native_fail(out, \"native owner is closed or invalid\");\n\
             \x20       slot->owner.reset();\n        slot->id = 0;\n\
             \x20       ku_native_integer(out, KU_INTEROP_NULL, 0);\n        return 0;\n"
        ),
        Operation::Method { name, projection } => {
            source.push_str(
                "        KuNativeSlot* slot = ku_native_find(args[0].integer);\n\
                 \x20       if (!slot) return ku_native_fail(out, \"native owner is closed or invalid\");\n"
            );
            let previous = format!("slot->owner->{name}({})", foreign_args(1));
            let call = match projection {
                Some(projected) => {
                    // The borrowed pointer cannot escape the bridge call. Copy
                    // any resulting string before the owner may be released.
                    source.push_str(&format!(
                        "        auto* borrowed = {previous};\n\
                         \x20       if (!borrowed) return ku_native_fail(out, \"native member returned no borrowed value\");\n"
                    ));
                    format!("borrowed->{projected}()")
                }
                None => previous,
            };
            match export.result {
                BridgeType::Int(_) => source.push_str(&format!("        return ku_native_checked_integer(out, {call});\n")),
                _ => emit_c_result(source, call, &export.result, None, ""),
            }
        }
    }
    source.push_str("    }\n");
    Ok(())
}

fn emit_integer_guard(source: &mut String, index: usize, integer: &Integer) {
    let min = integer.minimum;
    let max = integer.maximum;
    let outside = if integer.signed {
        format!("args[{index}].integer < (int64_t){min} || args[{index}].integer > (int64_t){max}")
    } else {
        format!("args[{index}].integer < 0 || (uint64_t)args[{index}].integer > (uint64_t){max}")
    };
    source.push_str(&format!(
        "        if ({outside}) return ku_native_fail(out, \"native integer argument out of range\");\n"
    ));
}

fn emit_c_result(
    source: &mut String,
    call: String,
    ty: &BridgeType,
    release: Option<&str>,
    cleanup: &str,
) {
    match ty {
        BridgeType::Void => source.push_str(&format!(
            "        {call};\n{cleanup}        ku_native_integer(out, KU_INTEROP_NULL, 0);\n        return 0;\n"
        )),
        BridgeType::Bool => source.push_str(&format!(
            "        int64_t result = ({call}) ? 1 : 0;\n{cleanup}        ku_native_integer(out, KU_INTEROP_BOOL, result);\n        return 0;\n"
        )),
        BridgeType::Int(integer) => {
            let spelling = &integer.spelling;
            source.push_str(&format!("        {spelling} result = {call};\n{cleanup}"));
            if !integer.signed {
                source.push_str("        if ((uint64_t)result > (uint64_t)INT64_MAX) return ku_native_fail(out, \"native integer result exceeds Ku int range\");\n");
            }
            source.push_str("        ku_native_integer(out, KU_INTEROP_INT, (int64_t)result);\n        return 0;\n");
        }
        BridgeType::Buffer { .. } => unreachable!("buffers are input-only"),
        BridgeType::Text => {
            if let Some(export) = release {
                source.push_str(&format!(
                    "        char* result = {call};\n\
                     \x20       int copied = ku_native_offer_owned(out, result, ku_native_drop_{export}, KU_NATIVE_TEXT_LIMIT);\n\
                     \x20       if (!copied && result) ku_native_drop_{export}(result);\n"
                ));
            } else {
                source.push_str(&format!(
                    "        const char* result = {call};\n        int copied = ku_native_text(out, result);\n"
                ));
            }
            source.push_str(&format!(
                "{cleanup}        if (!copied) return ku_native_fail(out, \"native C string result was null, too large or could not be copied\");\n        return 0;\n"
            ));
        }
    }
}

pub(super) fn wrapper_module(
    descriptor: &NativeDescriptor,
    exports: &[Export],
) -> KuResult<String> {
    let mut source = String::from("// Generated by ku package install. Review this module and ku.interop before committing.\n");
    for export in exports {
        let name = &export.name;
        let ty = export.result.ku_type();
        let params = export
            .args
            .iter()
            .enumerate()
            .map(|(index, param)| {
                let borrow = if matches!(param, BridgeType::Text | BridgeType::Buffer { .. }) {
                    "&"
                } else {
                    ""
                };
                format!("{borrow}arg{index}: {}", param.ku_type())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let args = export
            .args
            .iter()
            .enumerate()
            .map(|(index, _)| format!(", arg{index}"))
            .collect::<String>();
        source.push_str(&format!(
            "fn {name}({params}): {ty}! {{\n    return __ku_interop_{ty}(\"self\", \"{name}\"{args})\n}}\n"
        ));
    }
    if exports.is_empty() {
        return Err(err(
            "native_signature",
            format!("module '{}' contains no interop exports", descriptor.module),
        ));
    }
    Ok(source)
}

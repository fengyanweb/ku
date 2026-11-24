//! HTTP constants share the interpreter's source table. These helpers introduce
//! no process-global objects, startup allocation, threads or replacement ABI.

use crate::{
    error::KuResult,
    ir::{http_constant_object_table, IrExpr, IrProgram, IrType},
    stdlib::http::{HTTP_CODE_ALIASES, HTTP_STATUS_CODES},
};

use super::{c_expr, c_static_string, program_uses_intrinsic, unsupported, COutput};

/// Emit after the ordinary object runtime. Every returned table is an ordinary
/// independent owned KuObject; a read/clone never changes the module constants.
pub(super) fn emit(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    out.check()?;
    for (name, fields) in [
        (
            "status",
            HTTP_STATUS_CODES
                .iter()
                .map(|(field, code, _)| (*field, *code))
                .collect::<Vec<_>>(),
        ),
        ("code", HTTP_CODE_ALIASES.to_vec()),
    ] {
        if !program_uses_intrinsic(program, &format!("__ku_http_{name}_object")) {
            continue;
        }
        // Match the ordinary object load factor without an unnecessary grow.
        let capacity = if fields.len() <= 10 { 16 } else { 64 };
        out.push_str(&format!(
            "static KuObject* ku_http_{name}_object(void) {{\n  KuObject* object=ku_object_new({capacity}u);\n"
        ));
        for (field, code) in fields {
            out.push_str(&format!(
                "  {{ KuString key={}; KuValue value=ku_v_int({code});\n    if(!ku_object_try_set_owned(object,&key,&value)) {{\n      ku_string_drop(&key);ku_value_drop(&value);ku_object_drop(object);ku_object_hard_fail_oom();\n    }}\n  }}\n",
                c_static_string(field)
            ));
        }
        out.push_str("  return object;\n}\n");
    }
    if program_uses_intrinsic(program, "__ku_http_constant_get") {
        out.push_str(
            "static int64_t ku_http_constant_get(KuObject* object,KuString key) {\n  KuValue* value=ku_object_get(object,key);\n  if(!value || value->tag!=KU_INT)ku_object_internal_fail(\"HTTP constant field no longer has its checked integer type\");\n  return value->as.i;\n}\n",
        );
    }
    if program_uses_intrinsic(program, "http.statusText") {
        out.push_str("static KuString ku_http_status_string(int64_t code) {\n  switch(code) {\n");
        for (_, code, text) in HTTP_STATUS_CODES {
            out.push_str(&format!(
                "    case {code}: return {};\n",
                c_static_string(text)
            ));
        }
        out.push_str(&format!(
            "    default: return {};\n  }}\n}}\n",
            c_static_string("Unknown")
        ));
    }
    out.check()
}

/// Called by the existing intrinsic dispatcher; all inputs are checked before
/// emitting C so bypassing the language checker cannot produce a bad C cast.
pub(super) fn expression(name: &str, args: &[IrExpr]) -> KuResult<Option<String>> {
    match (name, args) {
        ("__ku_http_status_object", []) => Ok(Some("ku_http_status_object()".to_string())),
        ("__ku_http_code_object", []) => Ok(Some("ku_http_code_object()".to_string())),
        ("__ku_http_constant_get", [object, key])
            if matches!(&object.ty, IrType::Named(name) if http_constant_object_table(name).is_some())
                && key.ty == IrType::Str =>
        {
            Ok(Some(format!(
                "ku_http_constant_get({}, {})",
                c_expr(object)?,
                c_expr(key)?
            )))
        }
        ("http.statusText", [code]) if code.ty == IrType::Int => {
            Ok(Some(format!("ku_http_status_string({})", c_expr(code)?)))
        }
        (
            "__ku_http_status_object"
            | "__ku_http_code_object"
            | "__ku_http_constant_get"
            | "http.statusText",
            _,
        ) => Err(unsupported(
            "invalid native HTTP constant/statusText arguments",
        )),
        _ => Ok(None),
    }
}

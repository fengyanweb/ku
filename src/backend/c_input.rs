//! Bridge the shared stdin runtime to the native scalar and named Result ABIs.

use super::{
    c_expr, c_result_type, c_type, c_type_suffix, output::COutput, unsupported, IrCallKind, IrExpr,
    IrExprKind, IrInst, IrProgram, IrType, KuResult,
};
use crate::ir::input::{field_name, template_field_types, READ_LINE, READ_TEMPLATE};

const ADAPTER: &str = r#"
static KuError __ku_c_input_error(int status) {
  const char* code;
  const char* message;
  switch (status) {
    case 1: code = "eof"; message = "input ended"; break;
    case 3: code = "invalid_integer"; message = "expected a signed 64-bit decimal integer"; break;
    case 4: code = "allocation_failed"; message = "could not allocate input"; break;
    case 5: code = "unavailable"; message = "input is available only in synchronous CLI code"; break;
    default: code = "io_error"; message = "failed to read input or write its prompt"; break;
  }
  return ku_error_make(ku_string_static((const uint8_t*)"input", 5),
                       ku_string_static((const uint8_t*)code, strlen(code)),
                       ku_string_static((const uint8_t*)message, strlen(message)));
}
"#;

const TAKE_TEXT: &str = r#"
static KuString __ku_c_input_take_text(__ku_c_input_value* value) {
  KuString string = {value->bytes, value->len, value->len, KU_STRING_OWNED};
  value->bytes = NULL;
  return string;
}
"#;

const LINE_ADAPTER: &str = r#"
static KuResult_str __ku_c_input_line(KuString prompt) {
  if (__ku_handler_deadline)
    return (KuResult_str){false, {0}, __ku_c_input_error(5)};
  __ku_c_input_value value = {0};
  int status = __ku_c_input_read_raw((const char*)prompt.ptr, prompt.len,
                                     NULL, NULL, NULL, 1, &value);
  if (status != 0)
    return (KuResult_str){false, {0}, __ku_c_input_error(status)};
  return (KuResult_str){true, __ku_c_input_take_text(&value), {0}};
}
"#;

pub(super) fn emit(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    let mut uses_line = false;
    let mut template_payloads = Vec::new();
    for inst in program
        .functions
        .iter()
        .flat_map(|function| &function.blocks)
        .flat_map(|block| &block.instructions)
    {
        let IrInst::Temp { value, .. } = inst else {
            continue;
        };
        let IrExprKind::Call {
            kind: IrCallKind::Intrinsic(name),
            ..
        } = &value.kind
        else {
            continue;
        };
        if name == READ_LINE {
            uses_line = true;
        } else if name == READ_TEMPLATE {
            let IrType::Result(payload) = &value.ty else {
                return Err(unsupported("native input template must return a Result"));
            };
            if !template_payloads.contains(&payload.as_ref()) {
                template_payloads.push(payload.as_ref());
            }
        }
    }
    if !uses_line && template_payloads.is_empty() {
        return Ok(());
    }

    out.push_str(concat!(
        "\n#define KU_INPUT_SCOPE static\n",
        "#define KuInputValue __ku_c_input_value\n",
        "#define ku_input_read __ku_c_input_read_raw\n",
        "#define ku_input_free __ku_c_input_free_raw\n",
        include_str!("../runtime/input.c"),
        "\n#undef ku_input_free\n#undef ku_input_read\n",
        "#undef KuInputValue\n#undef KU_INPUT_SCOPE\n",
    ));
    out.push_str(ADAPTER);
    let uses_text = uses_line
        || template_payloads.iter().any(|payload| {
            template_field_types(payload).is_some_and(|fields| fields.contains(&IrType::Str))
        });
    if uses_text {
        out.push_str(TAKE_TEXT);
    }
    if uses_line {
        out.push_str(LINE_ADAPTER);
    }
    for payload in template_payloads {
        emit_template(out, payload)?;
    }
    out.check()
}

fn emit_template(out: &mut COutput, payload: &IrType) -> KuResult<()> {
    out.check()?;
    let fields = template_field_types(payload)
        .ok_or_else(|| unsupported("native input template has an invalid field type"))?;
    let count = fields.len();
    let segments = count + 1;
    let result = c_result_type(payload)?;
    let suffix = c_type_suffix(payload)?;
    let value_type = c_type(payload)?;
    let kinds = fields
        .iter()
        .map(|ty| match ty {
            IrType::Int => "1",
            _ => "0",
        })
        .collect::<Vec<_>>()
        .join(", ");

    out.push_str(&format!(
        "static {result} __ku_c_input_template_{suffix}(const KuString* text) {{\n\
         \x20 if (__ku_handler_deadline) return ({result}){{false, ({value_type}){{0}}, __ku_c_input_error(5)}};\n\
         \x20 const char* parts[{segments}]; size_t lengths[{segments}];\n\
         \x20 for (size_t i = 0; i < {segments}; ++i) {{ parts[i] = (const char*)text[i].ptr; lengths[i] = text[i].len; }}\n\
         \x20 static const uint8_t kinds[{count}] = {{ {kinds} }};\n\
         \x20 __ku_c_input_value fields[{count}] = {{0}};\n\
         \x20 int status = __ku_c_input_read_raw(NULL, 0, parts, lengths, kinds, {count}, fields);\n\
         \x20 if (status != 0) return ({result}){{false, ({value_type}){{0}}, __ku_c_input_error(status)}};\n\
         \x20 {value_type} value = {{0}};\n"
    ));
    for (index, ty) in fields.iter().enumerate() {
        let destination = if count == 1 {
            "value".to_owned()
        } else {
            format!("value.{}", field_name(index))
        };
        let source = if *ty == IrType::Str {
            format!("__ku_c_input_take_text(&fields[{index}])")
        } else {
            format!("fields[{index}].integer")
        };
        out.push_str(&format!("  {destination} = {source};\n"));
    }
    out.push_str(&format!(
        "  __ku_c_input_free_raw(fields, {count});\n\
         \x20 return ({result}){{true, value, {{0}}}};\n}}\n"
    ));
    out.check()
}

pub(super) fn expression(name: &str, args: &[IrExpr], ty: &IrType) -> KuResult<Option<String>> {
    match name {
        READ_LINE => {
            let prompt = match args {
                [] => "(KuString){0}".to_owned(),
                [prompt] if prompt.ty == IrType::Str => c_expr(prompt)?,
                _ => {
                    return Err(unsupported(
                        "native input expects one optional string prompt",
                    ))
                }
            };
            Ok(Some(format!("__ku_c_input_line({prompt})")))
        }
        READ_TEMPLATE => {
            let IrType::Result(payload) = ty else {
                return Err(unsupported("native input template must return a Result"));
            };
            let fields = template_field_types(payload)
                .ok_or_else(|| unsupported("native input template has an invalid field type"))?;
            if args.len() != fields.len() + 1
                || args
                    .iter()
                    .any(|arg| arg.ty != IrType::Str || !matches!(arg.kind, IrExprKind::Literal(_)))
            {
                return Err(unsupported(
                    "native input template expects static text segments",
                ));
            }
            let segments = args
                .iter()
                .map(c_expr)
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            let suffix = c_type_suffix(payload)?;
            Ok(Some(format!(
                "__ku_c_input_template_{suffix}((const KuString[]){{ {segments} }})"
            )))
        }
        _ => Ok(None),
    }
}

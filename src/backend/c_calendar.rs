//! Calendar operations share the interpreter's pure chrono engine through a
//! length-delimited native ABI. No interpreter state is linked or invoked.
use super::{
    c_expr, output::COutput, program_uses_intrinsic, unsupported, IrExpr, IrProgram, IrType,
    KuResult,
};

pub(super) fn uses_runtime(program: &IrProgram) -> bool {
    [
        "time.date",
        "time.datetime",
        "time.format",
        "time.parse",
        "time.parts",
        "time.weekday",
    ]
    .iter()
    .any(|name| program_uses_intrinsic(program, name))
}

pub(super) fn emit(out: &mut COutput, program: &IrProgram) {
    if !uses_runtime(program) {
        return;
    }
    out.push_str("#define KU_FEATURE_NATIVE_CALENDAR 1\n#define KuCalendarSliceV1 __ku_calendar_slice_v1\n#define KuCalendarInputV1 __ku_calendar_input_v1\n#define KuCalendarOutputV1 __ku_calendar_output_v1\n");
    out.push_str(include_str!(
        "../../crates/ku-native-calendar/include/ku_native_calendar.h"
    ));
    out.push_str(include_str!("c_calendar_runtime.c"));
    out.push_str("\n#undef KuCalendarSliceV1\n#undef KuCalendarInputV1\n#undef KuCalendarOutputV1\n#undef KU_NATIVE_CALENDAR_V1_H\n");
}

pub(super) fn intrinsic_expr(name: &str, args: &[IrExpr]) -> KuResult<Option<String>> {
    if matches!(name, "__ku_calendar_field_int" | "__ku_calendar_field_str") {
        let [object, key] = args else {
            return Err(unsupported("calendar field arity"));
        };
        return Ok(Some(format!(
            "{name}({}, {})",
            c_expr(object)?,
            c_expr(key)?
        )));
    }
    let operation = match name {
        "time.date" => match args {
            [] => 1,
            [time] | [time, _] if matches!(&time.ty, IrType::Named(n) if n == "__ku_time") => 2,
            [_, _, _] => 3,
            _ => return Err(unsupported("native time.date signature")),
        },
        "time.datetime" => 4,
        "time.format" => 5,
        "time.parse" => 6,
        "time.parts" => 7,
        "time.weekday" => {
            if matches!(args.first().map(|x| &x.ty), Some(IrType::Named(n)) if n == "__ku_time") {
                8
            } else {
                9
            }
        }
        _ => return Ok(None),
    };
    if operation == 9 {
        let [date, rest @ ..] = args else {
            return Err(unsupported("native weekday signature"));
        };
        let zone = rest
            .first()
            .map(c_expr)
            .transpose()?
            .unwrap_or_else(|| "ku_string_static((const uint8_t*)\"\",0)".into());
        return Ok(Some(format!(
            "__ku_calendar_weekday_date_{}({}, {}, {})",
            if args.len() == 1 { "bare" } else { "result" },
            c_expr(date)?,
            zone,
            args.len()
        )));
    }
    let mut numbers = Vec::new();
    let mut text = Vec::new();
    for arg in args {
        match &arg.ty {
            IrType::Int => numbers.push(c_expr(arg)?),
            IrType::Named(n) if n == "__ku_time" => {
                numbers.push(format!("ku_time_value_millis({})", c_expr(arg)?))
            }
            IrType::Str => {
                let expr = c_expr(arg)?;
                text.push(format!("__ku_calendar_slice({expr})"));
            }
            _ => {
                return Err(unsupported(format!(
                    "native {name} argument type {:?}",
                    arg.ty
                )))
            }
        }
    }
    let input = format!("(__ku_calendar_input_v1){{.size=sizeof(__ku_calendar_input_v1),.op={operation},.argc={},.numbers={{{}}},.text={{{}}}}}", args.len(), if numbers.is_empty(){"0".into()}else{numbers.join(",")}, if text.is_empty(){"{0}".into()}else{text.join(",")});
    let (kind, bare) = match operation {
        1 => ("object", true),
        2 | 3 => ("object", false),
        4 | 6 => ("time", false),
        5 => ("str", args.len() == 1),
        7 => ("object", args.len() == 1),
        8 => ("int", args.len() == 1),
        _ => unreachable!(),
    };
    Ok(Some(format!(
        "__ku_calendar_{kind}_{}({input})",
        if bare { "bare" } else { "result" }
    )))
}

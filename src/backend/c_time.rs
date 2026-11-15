//! Synchronous Time/Duration intrinsics. Their payload is a checked, copy-only
//! scalar; Result errors retain the ordinary KuError ownership convention.
use super::{
    c_expr, output::COutput, program_uses_intrinsic, unsupported, IrExpr, IrProgram, IrType,
    KuResult,
};

pub(super) fn uses_runtime(program: &IrProgram) -> bool {
    [
        "time.from_unix",
        "time.from_millis",
        "time.duration",
        "time.add",
        "time.sub",
        "time.diff",
        "time.compare",
        "time.is_leap",
        "time.days_in_month",
        "time.sleep",
    ]
    .iter()
    .any(|name| program_uses_intrinsic(program, name))
}

pub(super) fn emit(out: &mut COutput, program: &IrProgram) {
    if !uses_runtime(program) {
        return;
    }
    for (intrinsic, feature) in [
        ("time.duration", "__ku_time_duration_enabled"),
        ("time.days_in_month", "__ku_time_days_enabled"),
        ("time.sleep", "__ku_time_sleep_enabled"),
    ] {
        if program_uses_intrinsic(program, intrinsic) {
            out.push_str(&format!("#define {feature} 1\n"));
        }
    }
    out.push_str(include_str!("c_time_runtime.c"));
}

fn is_named(value: &IrExpr, name: &str) -> bool {
    matches!(&value.ty, IrType::Named(actual) if actual == name)
}

pub(super) fn intrinsic_expr(name: &str, args: &[IrExpr]) -> KuResult<Option<String>> {
    let rendered = match (name, args) {
        ("time.from_unix" | "time.from_millis", [value]) if value.ty == IrType::Int => {
            format!(
                "__ku_time_{}({})",
                name.strip_prefix("time.").unwrap(),
                c_expr(value)?
            )
        }
        ("time.duration", [value]) if value.ty == IrType::Int => {
            format!(
                "__ku_time_duration({}, ku_string_static((const uint8_t*)\"ms\", 2))",
                c_expr(value)?
            )
        }
        ("time.duration", [value, unit]) if value.ty == IrType::Int && unit.ty == IrType::Str => {
            format!("__ku_time_duration({}, {})", c_expr(value)?, c_expr(unit)?)
        }
        ("time.add" | "time.sub", [value, duration])
            if is_named(value, "__ku_time") && is_named(duration, "__ku_duration") =>
        {
            format!(
                "__ku_time_{}({}, {})",
                &name[5..],
                c_expr(value)?,
                c_expr(duration)?
            )
        }
        ("time.diff" | "time.compare", [left, right])
            if is_named(left, "__ku_time") && is_named(right, "__ku_time") =>
        {
            format!(
                "__ku_time_{}({}, {})",
                &name[5..],
                c_expr(left)?,
                c_expr(right)?
            )
        }
        ("time.millis", [duration]) if is_named(duration, "__ku_duration") => {
            format!("__ku_duration_value_millis({})", c_expr(duration)?)
        }
        ("time.is_leap", [year]) if year.ty == IrType::Int => {
            format!("__ku_time_is_leap({})", c_expr(year)?)
        }
        ("time.days_in_month", [year, month])
            if year.ty == IrType::Int && month.ty == IrType::Int =>
        {
            format!(
                "__ku_time_days_in_month({}, {})",
                c_expr(year)?,
                c_expr(month)?
            )
        }
        ("time.sleep", [value]) if value.ty == IrType::Int => {
            format!("__ku_time_sleep({})", c_expr(value)?)
        }
        ("time.sleep", [value]) if is_named(value, "__ku_duration") => {
            format!(
                "__ku_time_sleep(__ku_duration_value_millis({}))",
                c_expr(value)?
            )
        }
        (
            "time.from_unix" | "time.from_millis" | "time.duration" | "time.add" | "time.sub"
            | "time.diff" | "time.compare" | "time.is_leap" | "time.days_in_month" | "time.sleep",
            _,
        ) => {
            return Err(unsupported(format!("native C {name} received unsupported arguments; use native Time/Duration values with the documented signature")));
        }
        _ => return Ok(None),
    };
    Ok(Some(rendered))
}

pub(super) const DURATION_SCALAR: &str = r#"
#define __ku_duration_tag UINT64_C(0x4b75447572610001)
static void __ku_duration_validate(KuTime value) {
  if (value.tag != __ku_duration_tag) ku_time_fail("time: expected Duration value");
}
static KuString __ku_duration_kind(KuTime value) {
  __ku_duration_validate(value);
  return ku_string_static((const uint8_t*)"time.duration", 13);
}
static int64_t __ku_duration_value_millis(KuTime value) {
  __ku_duration_validate(value); return value.millis;
}
static bool __ku_duration_equal(KuTime left, KuTime right) {
  __ku_duration_validate(left); __ku_duration_validate(right);
  return left.millis == right.millis;
}
static bool __ku_duration_mixed_equal(KuTime left, KuTime right) {
  if (left.tag == KU_TIME_TAG) ku_time_validate(left);
  else __ku_duration_validate(left);
  if (right.tag == KU_TIME_TAG) ku_time_validate(right);
  else __ku_duration_validate(right);
  return left.tag == right.tag && left.millis == right.millis;
}
static void __ku_duration_print(KuTime value) {
  __ku_duration_validate(value);
  ku_output_printf("{ kind: time.duration, millis: %lld }", (long long)value.millis);
}
"#;

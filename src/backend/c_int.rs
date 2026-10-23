//! Checked i64 computations shared by native emitters.
//!
//! The caller emits this fixed helper set once after the standard C headers,
//! and decides how an arithmetic failure unwinds its own runtime. These pure
//! helpers do not allocate, exit, publish a Task state, or construct KuError.
//! This module alone does not change the existing synchronous C arithmetic.

use crate::backend::c::output::COutput;
use crate::error::KuResult;

pub(super) fn emit_runtime(out: &mut COutput) -> KuResult<()> {
    out.check()?;
    out.push_str(CHECKED_INT_RUNTIME);
    out.check()
}

const CHECKED_INT_RUNTIME: &str = r#"
/* Private checked int64_t computations. The standard <stdint.h> header must
 * precede this block. Emit this block once per translation unit. Inputs are
 * values; out is trusted, writable int64_t storage owned by generated code.
 * Failure never writes out. No errno, allocation, callback or process exit.
 * The caller maps OVERFLOW to "integer overflow" and DIV_ZERO to "division
 * by zero"; these statuses do not define public Error domain/code values. */
enum { KU_INT_OK = 0u, KU_INT_OVERFLOW = 1u, KU_INT_DIV_ZERO = 2u };
/* Keep the portable path for MSVC and compilers without these exact builtins.
 * This private feature macro is undefined before any user code is emitted. */
#if defined(__has_builtin)
# if __has_builtin(__builtin_add_overflow) && __has_builtin(__builtin_sub_overflow) && __has_builtin(__builtin_mul_overflow)
#  define KU_INT_USE_OVERFLOW_BUILTINS 1
# endif
#elif defined(__GNUC__) && __GNUC__ >= 5
# define KU_INT_USE_OVERFLOW_BUILTINS 1
#endif
#ifndef KU_INT_USE_OVERFLOW_BUILTINS
# define KU_INT_USE_OVERFLOW_BUILTINS 0
#endif
static uint32_t ku_int_neg(int64_t value, int64_t* out) {
  if (value == INT64_MIN) return KU_INT_OVERFLOW;
  *out = -value;
  return KU_INT_OK;
}
static uint32_t ku_int_add(int64_t left, int64_t right, int64_t* out) {
#if KU_INT_USE_OVERFLOW_BUILTINS
  /* A builtin writes its wrapped result even on overflow. Use a private
   * temporary so failure still leaves out (including either input alias) intact. */
  int64_t result;
  if (__builtin_add_overflow(left, right, &result)) return KU_INT_OVERFLOW;
  *out = result;
#else
  /* MAX-right is evaluated only for positive right; MIN-right only for
   * negative right. Both bounds themselves are representable. */
  if ((right > 0 && left > INT64_MAX - right)
      || (right < 0 && left < INT64_MIN - right)) return KU_INT_OVERFLOW;
  *out = left + right;
#endif
  return KU_INT_OK;
}
static uint32_t ku_int_sub(int64_t left, int64_t right, int64_t* out) {
#if KU_INT_USE_OVERFLOW_BUILTINS
  int64_t result;
  if (__builtin_sub_overflow(left, right, &result)) return KU_INT_OVERFLOW;
  *out = result;
#else
  /* Do not negate right: right may be INT64_MIN. */
  if ((right > 0 && left < INT64_MIN + right)
      || (right < 0 && left > INT64_MAX + right)) return KU_INT_OVERFLOW;
  *out = left - right;
#endif
  return KU_INT_OK;
}
static uint32_t ku_int_mul(int64_t left, int64_t right, int64_t* out) {
#if KU_INT_USE_OVERFLOW_BUILTINS
  /* Match add/sub: builtin overflow must not alter out or an aliased input. */
  int64_t result;
  if (__builtin_mul_overflow(left, right, &result)) return KU_INT_OVERFLOW;
  *out = result;
#else
  /* Every divisor below is nonzero. INT64_MIN is divided only by a
   * positive value; MAX/negative is always representable. In the final
   * branch, truncation toward zero gives ceil(MAX/right), the valid lower
   * bound for a negative left. No abs(MIN), MIN/-1 or speculative product. */
  if (left > 0) {
    if (right > 0) {
      if (left > INT64_MAX / right) return KU_INT_OVERFLOW;
    } else if (right < 0) {
      if (right < INT64_MIN / left) return KU_INT_OVERFLOW;
    }
  } else if (left < 0) {
    if (right > 0) {
      if (left < INT64_MIN / right) return KU_INT_OVERFLOW;
    } else if (right < 0) {
      if (left < INT64_MAX / right) return KU_INT_OVERFLOW;
    }
  }
  *out = left * right;
#endif
  return KU_INT_OK;
}
#undef KU_INT_USE_OVERFLOW_BUILTINS
static uint32_t ku_int_div(int64_t left, int64_t right, int64_t* out) {
  if (right == 0) return KU_INT_DIV_ZERO;
  if (left == INT64_MIN && right == -1) return KU_INT_OVERFLOW;
  *out = left / right;
  return KU_INT_OK;
}
static uint32_t ku_int_rem(int64_t left, int64_t right, int64_t* out) {
  if (right == 0) return KU_INT_DIV_ZERO;
  /* Ku follows checked_rem: MIN%-1 is overflow, not a special-cased zero. */
  if (left == INT64_MIN && right == -1) return KU_INT_OVERFLOW;
  *out = left % right;
  return KU_INT_OK;
}
"#;

#ifndef KU_NATIVE_CALENDAR_V1_H
#define KU_NATIVE_CALENDAR_V1_H
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

/* Private generated-runtime ABI. Objects and input ranges stay valid for the
 * call. Objects are aligned; output storage never overlaps input storage.
 * Read-only inputs may alias each other. Text is UTF-8 by explicit byte
 * length; embedded NUL is data, never a terminator. */
typedef struct KuCalendarSliceV1 {
  const uint8_t* data;
  size_t len;
} KuCalendarSliceV1;

typedef struct KuCalendarInputV1 {
  uint32_t size;
  uint32_t op;
  uint32_t argc;
  uint32_t reserved;
  int64_t numbers[8];
  KuCalendarSliceV1 text[3];
} KuCalendarInputV1;

typedef struct KuCalendarOutputV1 {
  uint32_t size;
  uint32_t status;
  int64_t numbers[10];
  KuCalendarSliceV1 text;
  KuCalendarSliceV1 code;
  KuCalendarSliceV1 message;
  void* owner;
} KuCalendarOutputV1;

enum {
  __ku_calendar_status_ok = 0,
  __ku_calendar_status_error = 1,
  __ku_calendar_status_invalid_abi = 2,
  __ku_calendar_status_panic = 255,
  __ku_calendar_op_date_now = 1,
  __ku_calendar_op_date_at = 2,
  __ku_calendar_op_date_ymd = 3,
  __ku_calendar_op_datetime = 4,
  __ku_calendar_op_format = 5,
  __ku_calendar_op_parse = 6,
  __ku_calendar_op_parts = 7,
  __ku_calendar_op_weekday_time = 8,
  __ku_calendar_op_weekday_date = 9
};

uint32_t __ku_calendar_abi_version(void);
uint32_t __ku_calendar_v1_build_id(const uint8_t** data, size_t* len);
/* Caller initializes output to zero except size=sizeof(output), and input.size
 * likewise. A live owner must be released before output is reused. For a valid
 * empty output, the return value equals output.status. A malformed output or
 * live owner is rejected without changing output or discarding its owner.
 * All returned byte views are valid until release. The caller copies bytes to
 * its own allocator before releasing; never free these views using C free().
 * No user Result/fatal policy is encoded here; the generated adapter retains
 * each public overload's original policy. Numeric results need no owner. */
uint32_t __ku_calendar_call_v1(const KuCalendarInputV1* input,
                             KuCalendarOutputV1* output);
/* Matched Rust release. Idempotent on an initialized, already released output.
 * It also clears status, numbers and every returned byte view. */
uint32_t __ku_calendar_release_v1(KuCalendarOutputV1* output);

#ifdef __cplusplus
}
#endif
#endif

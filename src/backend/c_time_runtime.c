/* Optional synchronous scalar time operations, emitted after Result ABIs.
 * No clock/provider, worker, allocation, or calendar initialization at startup. */
#if defined(__ku_time_sleep_enabled) && defined(_WIN32)
/* Clock-only programs intentionally do not include windows.h. Declare the
 * exact kernel32 ABI without introducing that header's user-name macros. */
#if defined(_MSC_VER)
__declspec(dllimport) void __stdcall Sleep(unsigned long milliseconds);
#elif defined(__GNUC__) || defined(__clang__)
__attribute__((dllimport)) void __attribute__((stdcall)) Sleep(unsigned long milliseconds);
#else
void Sleep(unsigned long milliseconds);
#endif
#endif
static KuError __ku_time_error(const char* code, const char* message) {
  return ku_error_make(ku_string_static((const uint8_t*)"time", 4),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}
static KuTime __ku_time_fail_scalar(const char* static_message) {
  /* Reuse the same-thread static fatal-error transport used by HTTP JSON.
   * The immediately adjacent IR SyncGuard retires owners before the request
   * root converts this failure to a response. No user Result is invented. */
  __ku_sync_raise_http_json((const uint8_t*)static_message, strlen(static_message));
  return (KuTime){0};
}
static KuTime __ku_time_from_unix(int64_t seconds) {
  int64_t millis;
  if (ku_int_mul(seconds, INT64_C(1000), &millis) != KU_INT_OK)
    return __ku_time_fail_scalar("time/invalid_time: time calculation overflow");
  return (KuTime){KU_TIME_TAG, millis};
}
static KuTime __ku_time_from_millis(int64_t millis) {
  /* Same UTC representable range as the locked chrono NaiveDateTime, inclusive.
   * Unlike from_unix, this API validates its calendar representation. */
  if (millis < -INT64_C(8334601228800000) || millis > INT64_C(8210266876799999))
    return __ku_time_fail_scalar("time/invalid_time: time millis outside supported range");
  return (KuTime){KU_TIME_TAG, millis};
}
static KuTime __ku_time_add(KuTime time, KuTime duration) {
  ku_time_validate(time); __ku_duration_validate(duration);
  int64_t millis;
  if (ku_int_add(time.millis, duration.millis, &millis) != KU_INT_OK)
    return __ku_time_fail_scalar("time/invalid_time: time calculation overflow");
  return (KuTime){KU_TIME_TAG, millis};
}
static KuTime __ku_time_sub(KuTime time, KuTime duration) {
  ku_time_validate(time); __ku_duration_validate(duration);
  int64_t millis;
  if (ku_int_sub(time.millis, duration.millis, &millis) != KU_INT_OK)
    return __ku_time_fail_scalar("time/invalid_time: time calculation overflow");
  return (KuTime){KU_TIME_TAG, millis};
}
static KuTime __ku_time_diff(KuTime left, KuTime right) {
  ku_time_validate(left); ku_time_validate(right);
  int64_t millis;
  if (ku_int_sub(left.millis, right.millis, &millis) != KU_INT_OK)
    return __ku_time_fail_scalar("time/invalid_duration: time calculation overflow");
  return (KuTime){__ku_duration_tag, millis};
}
static int64_t __ku_time_compare(KuTime left, KuTime right) {
  ku_time_validate(left); ku_time_validate(right);
  return left.millis < right.millis ? -1 : left.millis > right.millis ? 1 : 0;
}
static bool __ku_time_is_leap(int64_t year) {
  return (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
}
#if defined(__ku_time_duration_enabled)
static KuResult_duration __ku_time_duration(int64_t value, KuString unit) {
  if (value < 0) return (KuResult_duration){false, {0},
      __ku_time_error("invalid_duration", "duration must be >= 0")};
  int64_t multiplier = 0;
  if (ku_string_equal(unit, ku_string_static((const uint8_t*)"ms", 2))) multiplier = 1;
  else if (ku_string_equal(unit, ku_string_static((const uint8_t*)"s", 1))) multiplier = 1000;
  else if (ku_string_equal(unit, ku_string_static((const uint8_t*)"m", 1))) multiplier = 60000;
  else if (ku_string_equal(unit, ku_string_static((const uint8_t*)"h", 1))) multiplier = 3600000;
  else if (ku_string_equal(unit, ku_string_static((const uint8_t*)"d", 1))) multiplier = 86400000;
  else {
    KuString prefix = ku_string_static((const uint8_t*)"unsupported duration unit '", 27);
    KuString message = ku_string_concat(prefix, unit);
    KuString full = ku_string_concat(message, ku_string_static((const uint8_t*)"'", 1));
    ku_string_drop(&message);
    KuError error = __ku_time_error("invalid_duration", ""); error.message = full;
    return (KuResult_duration){false, {0}, error};
  }
  int64_t millis;
  if (ku_int_mul(value, multiplier, &millis) != KU_INT_OK)
    return (KuResult_duration){false, {0}, __ku_time_error("invalid_duration", "time calculation overflow")};
  return (KuResult_duration){true, {__ku_duration_tag, millis}, {0}};
}
#endif
#if defined(__ku_time_days_enabled)
static KuResult_int __ku_time_days_error(const char* code, const char* format,
                                     int64_t year, int64_t month) {
  char text[96];
  int len = snprintf(text, sizeof(text), format, (long long)year, (long long)month);
  if (len < 0 || (size_t)len >= sizeof(text)) ku_time_fail("time: error formatting failed");
  KuError error = __ku_time_error(code, "");
  error.message = ku_string_alloc((size_t)len);
  if (len) memcpy(error.message.ptr, text, (size_t)len);
  return (KuResult_int){false, 0, error};
}
static KuResult_int __ku_time_days_in_month(int64_t year, int64_t month) {
  if (year < INT32_MIN || year > INT32_MAX)
    return (KuResult_int){false, 0, __ku_time_error("invalid_time", "int is outside supported date range")};
  if (month < 0 || (uint64_t)month > UINT32_MAX)
    return (KuResult_int){false, 0, __ku_time_error("invalid_time", "int must be non-negative")};
  if (month == 0 || month > 12)
    return __ku_time_days_error("invalid_date", "invalid month: %lld", month, 0);
  if (year < -262143 || year > 262142)
    return __ku_time_days_error("invalid_date", "invalid date: %04lld-%02lld-01", year, month);
  if (year == 262142 && month == 12)
    return __ku_time_days_error("invalid_date", "invalid date: %04lld-%02lld-01", year + 1, 1);
  static const uint8_t days[] = {31,28,31,30,31,30,31,31,30,31,30,31};
  return (KuResult_int){true, (int64_t)days[month - 1] + (month == 2 && __ku_time_is_leap(year)), {0}};
}
#endif
#if defined(__ku_time_sleep_enabled)
static KuResult_null __ku_time_sleep(int64_t millis) {
  if (millis < 0) return (KuResult_null){false, 0,
      __ku_time_error("invalid_duration", "duration must be >= 0")};
  int64_t started = ku_time_steady_millis();
  int64_t remaining = millis;
  while (remaining > 0) {
    if (__ku_handler_timeout_poll())
      return (KuResult_null){false, 0, __ku_time_error("timeout", "handler deadline exceeded")};
    int64_t chunk = remaining > 10 ? 10 : remaining;
#if defined(_WIN32)
    Sleep((unsigned long)chunk);
#else
    struct timespec requested = {0, (long)(chunk * INT64_C(1000000))}, rest;
    while (nanosleep(&requested, &rest) != 0) {
      if (errno != EINTR)
        return (KuResult_null){false, 0, __ku_time_error("sleep_error", "operating system sleep failed")};
      if (__ku_handler_timeout_poll())
        return (KuResult_null){false, 0, __ku_time_error("timeout", "handler deadline exceeded")};
      requested = rest;
    }
#endif
    int64_t now = ku_time_steady_millis();
    if (now < started) ku_time_fail("time.sleep: monotonic clock moved backwards");
    int64_t elapsed = now - started;
    remaining = elapsed >= millis ? 0 : millis - elapsed;
  }
  /* Include the final slice and zero-length calls in the original deadline. */
  if (__ku_handler_timeout_poll())
    return (KuResult_null){false, 0, __ku_time_error("timeout", "handler deadline exceeded")};
  return (KuResult_null){true, 0, {0}};
}
#endif
#undef __ku_time_duration_enabled
#undef __ku_time_days_enabled
#undef __ku_time_sleep_enabled

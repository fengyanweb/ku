/* Test-only: included into EACH C translation unit by the required harness.
 * Do not infer instrumentation merely from another linked probe object. */
#ifndef __has_feature
#define __has_feature(feature) 0
#endif
#if !defined(__clang__) || !__has_feature(thread_sanitizer)
#error KU native Task TSan gate requires Clang ThreadSanitizer instrumentation
#endif
#if __has_feature(address_sanitizer)
#error KU native Task TSan gate must not share the AddressSanitizer process
#endif

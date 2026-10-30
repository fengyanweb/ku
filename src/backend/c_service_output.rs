//! Bounded output used by native services; ordinary programs remain synchronous.

use super::output::COutput;

pub(super) fn emit(out: &mut COutput, service: bool) {
    if service {
        out.push_str("#define KU_SERVICE_OUTPUT_SUPPORTED 1\n");
        out.push_str(include_str!("c_service_output.c"));
    } else {
        // Keep ordinary artifacts free from even dormant thread runtimes. Pure
        // Task output uses this same small synchronous error-reporting ABI.
        out.push_str(
            r#"#include <stdarg.h>
static int ku_output_stream_write(FILE* stream, const void* bytes, size_t length) {
  const unsigned char* cursor = (const unsigned char*)bytes;
  while (length) {
    size_t count = fwrite(cursor, 1, length, stream);
    if (!count) return -1;
    cursor += count; length -= count;
  }
  return 0;
}
static int ku_output_write(const void* bytes, size_t length) {
  return ku_output_stream_write(stdout, bytes, length);
}
static int ku_output_flush(void) { return fflush(stdout); }
static int ku_output_printf(const char* format, ...) {
  va_list arguments; va_start(arguments, format);
  int result = vfprintf(stdout, format, arguments);
  va_end(arguments);
  return result < 0 ? -1 : 0;
}
"#,
        );
    }
    out.push('\n');
}

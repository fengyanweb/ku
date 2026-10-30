/* Linux-only diagnostic footer, appended AFTER ordinary CLI-generated C.
 * Link this separate diagnostic ELF with -Wl,--wrap=pthread_create.
 * The Python prepare command emits KU_COST_HAS_* from actual static definitions.
 * This is never a scoring build: entry references can retain otherwise-dead
 * functions and change code layout. The original generated prefix stays intact.
 */
#if !defined(__linux__)
#error "HTTP thread-role diagnostic is Linux-only"
#endif
#include <sys/prctl.h>
#include <sys/syscall.h>
/* Generated C may select strict POSIX declarations before this footer. */
extern long syscall(long number, ...);
extern int __real_pthread_create(pthread_t*, const pthread_attr_t*,
                                void* (*)(void*), void*);

typedef struct KuCostThreadStart {
  void* (*entry)(void*);
  void* argument;
  const char* role;
} KuCostThreadStart;

static const char* ku_cost_thread_role(void* (*entry)(void*)) {
#if KU_COST_HAS_HTTP_WORKER
  if (entry == ku_http_worker) return "kuS";
#endif
#if KU_COST_HAS_HTTP_OWNER
  if (entry == ku_http_loop_thread) return "kuO";
#endif
#if KU_COST_HAS_TASK_BLOCKING
  if (entry == ku_task_blocking_thread) return "kuB";
#endif
#if KU_COST_HAS_TASK_DRIVER
  if (entry == ku_task_driver_thread) return "kuD";
#endif
#if KU_COST_HAS_OUTPUT
  if (entry == ku_output_thread) return "kuW";
#endif
  return "kuU";
}

static void* ku_cost_thread_start(void* raw) {
  int previous_errno = errno;
  KuCostThreadStart start = *(KuCostThreadStart*)raw;
  free(raw);
  long tid = syscall(SYS_gettid);
  char comm[16];
  int length = snprintf(comm, sizeof(comm), "%s:%ld", start.role, tid);
  /* Linux pid_t fits the three-character role + ':' + decimal TID + NUL.
   * One naming attempt only, without output or per-request instrumentation.
   * An unwrapped DSO child inherits this name but has a different TID; the
   * observer rejects that inherited tag rather than guessing its role.
   */
  if (tid > 0 && length > 0 && (size_t)length < sizeof(comm))
    (void)prctl(PR_SET_NAME, (unsigned long)comm, 0UL, 0UL, 0UL);
  errno = previous_errno;
  return start.entry(start.argument);
}

int __wrap_pthread_create(pthread_t* thread, const pthread_attr_t* attributes,
                         void* (*entry)(void*), void* argument) {
  int previous_errno = errno;
  KuCostThreadStart* start = (KuCostThreadStart*)malloc(sizeof(*start));
  if (!start) { errno = previous_errno; return ENOMEM; }
  *start = (KuCostThreadStart){entry, argument, ku_cost_thread_role(entry)};
  int status = __real_pthread_create(thread, attributes, ku_cost_thread_start, start);
  if (status) free(start);
  errno = previous_errno;
  return status;
}

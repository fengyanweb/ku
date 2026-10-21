/* Service stdout/stderr have one bounded owner. A blocked stdio call retains
 * this static ring and thread until the process exit boundary; producers never
 * call stdio while the service output facility is active. */
#include <stdarg.h>

#if defined(KU_SERVICE_OUTPUT_SUPPORTED)
#define KU_FEATURE_SERVICE_OUTPUT 1
#define KU_SERVICE_OUTPUT_SLOTS 256u
#define KU_SERVICE_OUTPUT_RECORD_BYTES 4096u
#if defined(_WIN32)
#include <process.h>
typedef CRITICAL_SECTION KuOutputMutex;
typedef CONDITION_VARIABLE KuOutputCondition;
#else
#include <pthread.h>
#include <signal.h>
#endif
typedef struct KuServiceOutputSnapshot {
  size_t queued_records, queued_bytes, peak_bytes;
  size_t dropped_records, dropped_bytes, failures, written_bytes;
  int active, running, closing;
} KuServiceOutputSnapshot;
typedef struct KuServiceOutputRecord {
  size_t bytes;
  FILE* stream;
  unsigned char data[KU_SERVICE_OUTPUT_RECORD_BYTES];
} KuServiceOutputRecord;
typedef struct KuServiceOutput {
#if defined(_WIN32)
  KuOutputMutex mutex;
  KuOutputCondition condition;
  HANDLE thread;
#else
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  pthread_t thread;
  struct sigaction previous_sigpipe;
  int sigpipe_changed;
#endif
  KuServiceOutputRecord records[KU_SERVICE_OUTPUT_SLOTS];
  KuServiceOutputSnapshot metrics;
  size_t head;
  int initialized, thread_created, done;
} KuServiceOutput;
static KuServiceOutput ku_service_output;
static size_t ku_runtime_thread_stack_bytes(void);

static void ku_output_lock(void) {
#if defined(_WIN32)
  EnterCriticalSection(&ku_service_output.mutex);
#else
  if (pthread_mutex_lock(&ku_service_output.mutex)) _Exit(79);
#endif
}
static void ku_output_unlock(void) {
#if defined(_WIN32)
  LeaveCriticalSection(&ku_service_output.mutex);
#else
  if (pthread_mutex_unlock(&ku_service_output.mutex)) _Exit(79);
#endif
}
static void ku_output_signal(void) {
#if defined(_WIN32)
  WakeAllConditionVariable(&ku_service_output.condition);
#else
  if (pthread_cond_broadcast(&ku_service_output.condition)) _Exit(79);
#endif
}
static unsigned long long ku_output_now_ms(void) {
#if defined(_WIN32)
  return GetTickCount64();
#else
  struct timespec value = {0};
  if (clock_gettime(CLOCK_MONOTONIC, &value)) _Exit(79);
  return (unsigned long long)value.tv_sec * 1000ULL + (unsigned long long)value.tv_nsec / 1000000ULL;
#endif
}
static int ku_output_wait(unsigned long long deadline) {
  if (deadline == UINT64_MAX) {
#if defined(_WIN32)
    return SleepConditionVariableCS(&ku_service_output.condition, &ku_service_output.mutex, INFINITE) ? 1 : -1;
#else
    return pthread_cond_wait(&ku_service_output.condition, &ku_service_output.mutex) ? -1 : 1;
#endif
  }
#if defined(_WIN32)
  unsigned long long now = ku_output_now_ms();
  if (now >= deadline) return 0;
  unsigned long long remaining = deadline - now;
  DWORD timeout = remaining > (unsigned long long)(INFINITE - 1) ? INFINITE - 1 : (DWORD)remaining;
  if (SleepConditionVariableCS(&ku_service_output.condition, &ku_service_output.mutex, timeout)) return 1;
  return GetLastError() == ERROR_TIMEOUT ? 0 : -1;
#else
  unsigned long long now = ku_output_now_ms();
  if (now >= deadline) return 0;
  unsigned long long remaining = deadline - now;
  struct timespec timeout = {0};
#if defined(__APPLE__)
  timeout.tv_sec = (time_t)(remaining / 1000ULL);
  timeout.tv_nsec = (long)(remaining % 1000ULL) * 1000000L;
  int result = pthread_cond_timedwait_relative_np(&ku_service_output.condition, &ku_service_output.mutex, &timeout);
#else
  if (clock_gettime(CLOCK_MONOTONIC, &timeout)) return -1;
  timeout.tv_sec += (time_t)(remaining / 1000ULL);
  timeout.tv_nsec += (long)(remaining % 1000ULL) * 1000000L;
  if (timeout.tv_nsec >= 1000000000L) { timeout.tv_sec++; timeout.tv_nsec -= 1000000000L; }
  int result = pthread_cond_timedwait(&ku_service_output.condition, &ku_service_output.mutex, &timeout);
#endif
  return result == 0 ? 1 : result == ETIMEDOUT ? 0 : -1;
#endif
}
static void ku_output_drop_locked(size_t bytes) {
  KuServiceOutputSnapshot* metrics = &ku_service_output.metrics;
  if (metrics->dropped_records != SIZE_MAX) metrics->dropped_records++;
  metrics->dropped_bytes = bytes > SIZE_MAX - metrics->dropped_bytes ? SIZE_MAX : metrics->dropped_bytes + bytes;
}

static void ku_output_worker(void) {
  KuServiceOutput* output = &ku_service_output;
#if !defined(_WIN32)
  /* A closed log collector is an output failure, not a process-wide SIGPIPE. */
  sigset_t blocked;
  if (sigemptyset(&blocked) || sigaddset(&blocked, SIGPIPE)
      || pthread_sigmask(SIG_BLOCK, &blocked, NULL)) _Exit(79);
#endif
  ku_output_lock();
  for (;;) {
    while (!output->metrics.queued_records && !output->metrics.closing) {
      if (ku_output_wait(UINT64_MAX) < 0) _Exit(79);
    }
    if (!output->metrics.queued_records && output->metrics.closing) break;
    KuServiceOutputRecord* record = &output->records[output->head];
    size_t bytes = record->bytes;
    FILE* stream = record->stream;
    output->metrics.running = 1;
    ku_output_unlock();
    /* The head remains charged and cannot be reused while fwrite/fflush holds
       its pointer. No output mutex is held during either blocking call. */
    size_t written = 0;
    while (written < bytes) {
      size_t count = fwrite(record->data + written, 1, bytes - written, stream);
      if (!count) break;
      written += count;
    }
    int failed = written != bytes || fflush(stream) != 0;
    ku_output_lock();
    if (failed && output->metrics.failures != SIZE_MAX) output->metrics.failures++;
    if (!failed) output->metrics.written_bytes = written > SIZE_MAX - output->metrics.written_bytes
        ? SIZE_MAX : output->metrics.written_bytes + written;
    output->metrics.queued_bytes -= bytes;
    output->metrics.queued_records--;
    output->metrics.running = 0;
    output->head = (output->head + 1u) % KU_SERVICE_OUTPUT_SLOTS;
    ku_output_signal();
  }
  output->done = 1;
  ku_output_signal();
  ku_output_unlock();
}
#if defined(_WIN32)
static unsigned __stdcall ku_output_thread(void* raw) { (void)raw; ku_output_worker(); return 0; }
#else
static void* ku_output_thread(void* raw) { (void)raw; ku_output_worker(); return NULL; }
#endif

/* Called before user main and therefore before service threads exist. */
static int ku_service_output_start(void) {
  KuServiceOutput* output = &ku_service_output;
  if (output->initialized) return 1;
  const char* mode = getenv("KU_SERVICE_OUTPUT");
  if (mode && !strcmp(mode, "sync")) return 1;
  if (mode && strcmp(mode, "bounded")) return 0;
#if defined(_WIN32)
  if (!InitializeCriticalSectionAndSpinCount(&output->mutex, 0)) return 0;
  InitializeConditionVariable(&output->condition);
#else
  if (pthread_mutex_init(&output->mutex, NULL)) return 0;
  pthread_condattr_t attributes;
  if (pthread_condattr_init(&attributes)) { pthread_mutex_destroy(&output->mutex); return 0; }
#if !defined(__APPLE__)
  if (pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC)) {
    pthread_condattr_destroy(&attributes); pthread_mutex_destroy(&output->mutex); return 0;
  }
#endif
  int condition = pthread_cond_init(&output->condition, &attributes);
  pthread_condattr_destroy(&attributes);
  if (condition) { pthread_mutex_destroy(&output->mutex); return 0; }
  /* Some POSIX platforms deliver pipe SIGPIPE process-wide rather than only
     to the writing thread. A service treats a closed collector as EPIPE. This
     policy is installed before any service threads, never for ordinary/sync
     programs; the socket runtimes also keep their existing no-signal writes. */
  struct sigaction ignored = {0};
  ignored.sa_handler = SIG_IGN;
  if (sigemptyset(&ignored.sa_mask)
      || sigaction(SIGPIPE, &ignored, &output->previous_sigpipe)) {
    pthread_cond_destroy(&output->condition); pthread_mutex_destroy(&output->mutex); return 0;
  }
  output->sigpipe_changed = 1;
#endif
  output->initialized = 1;
  output->metrics.active = 1;
  size_t stack_bytes = ku_runtime_thread_stack_bytes();
#if defined(_WIN32)
  output->thread = (HANDLE)_beginthreadex(NULL, (unsigned int)stack_bytes, ku_output_thread, NULL, STACK_SIZE_PARAM_IS_A_RESERVATION, NULL);
  output->thread_created = output->thread != NULL;
#else
  pthread_attr_t attributes_thread;
  int status = pthread_attr_init(&attributes_thread);
  if (!status) {
    status = pthread_attr_setstacksize(&attributes_thread, stack_bytes);
    if (!status) status = pthread_create(&output->thread, &attributes_thread, ku_output_thread, NULL);
    pthread_attr_destroy(&attributes_thread);
  }
  output->thread_created = status == 0;
#endif
  if (!output->thread_created) {
    output->metrics.active = 0;
    output->initialized = 0;
#if defined(_WIN32)
    DeleteCriticalSection(&output->mutex);
#else
    if (output->sigpipe_changed) {
      sigaction(SIGPIPE, &output->previous_sigpipe, NULL); output->sigpipe_changed = 0;
    }
    pthread_cond_destroy(&output->condition); pthread_mutex_destroy(&output->mutex);
#endif
    return 0;
  }
  return 1;
}
static KuServiceOutputSnapshot ku_service_output_snapshot(void) {
  if (!ku_service_output.initialized) return (KuServiceOutputSnapshot){0};
  ku_output_lock();
  KuServiceOutputSnapshot snapshot = ku_service_output.metrics;
  ku_output_unlock();
  return snapshot;
}
static int ku_service_output_finish(unsigned long long deadline) {
  KuServiceOutput* output = &ku_service_output;
  if (!output->initialized || !output->thread_created) return 1;
  ku_output_lock();
  output->metrics.closing = 1;
  ku_output_signal();
  while (!output->done) {
    int waited = ku_output_wait(deadline);
    if (waited <= 0) { ku_output_unlock(); return 0; }
  }
  ku_output_unlock();
#if defined(_WIN32)
  if (WaitForSingleObject(output->thread, INFINITE) != WAIT_OBJECT_0) return 0;
  CloseHandle(output->thread);
#else
  if (pthread_join(output->thread, NULL)) return 0;
#endif
  output->thread_created = 0;
  return 1;
}
static int ku_output_stream_write(FILE* stream, const void* bytes, size_t length) {
  if (!length) return 0;
  KuServiceOutput* output = &ku_service_output;
  if (output->initialized && output->metrics.active) {
    ku_output_lock();
    if (length > KU_SERVICE_OUTPUT_RECORD_BYTES || output->metrics.closing
        || output->metrics.queued_records == KU_SERVICE_OUTPUT_SLOTS) {
      ku_output_drop_locked(length);
    } else {
      size_t index = (output->head + output->metrics.queued_records) % KU_SERVICE_OUTPUT_SLOTS;
      KuServiceOutputRecord* record = &output->records[index];
      memcpy(record->data, bytes, length); record->bytes = length; record->stream = stream;
      output->metrics.queued_records++; output->metrics.queued_bytes += length;
      if (output->metrics.queued_bytes > output->metrics.peak_bytes) output->metrics.peak_bytes = output->metrics.queued_bytes;
      ku_output_signal();
    }
    ku_output_unlock();
    return 0;
  }
  const unsigned char* cursor = (const unsigned char*)bytes;
  while (length) {
    size_t count = fwrite(cursor, 1, length, stream);
    if (!count) return -1;
    cursor += count; length -= count;
  }
  return 0;
}
static int ku_service_output_exit(int code) {
  unsigned long long now = ku_output_now_ms();
  unsigned long long deadline = UINT64_MAX - now < 1000ULL ? UINT64_MAX : now + 1000ULL;
  if (!ku_service_output_finish(deadline)) _Exit(code);
  /* A failed stdio flush may leave bytes buffered. libc exit would retry that
     write on main, reintroducing blocking or SIGPIPE outside the output owner. */
  if (ku_service_output_snapshot().failures) _Exit(code);
  return code;
}
#else
static int ku_output_stream_write(FILE* stream, const void* bytes, size_t length) {
  const unsigned char* cursor = (const unsigned char*)bytes;
  while (length) {
    size_t count = fwrite(cursor, 1, length, stream);
    if (!count) return -1;
    cursor += count; length -= count;
  }
  return 0;
}
#endif
static int ku_output_write(const void* bytes, size_t length) { return ku_output_stream_write(stdout, bytes, length); }
static int ku_output_flush(void) {
#if defined(KU_SERVICE_OUTPUT_SUPPORTED)
  if (ku_service_output.initialized && ku_service_output.metrics.active) return 0;
#endif
  return fflush(stdout);
}
static int ku_output_printf(const char* format, ...) {
  va_list arguments;
  va_start(arguments, format);
  int status = 0;
#if defined(KU_SERVICE_OUTPUT_SUPPORTED)
  if (ku_service_output.initialized && ku_service_output.metrics.active) {
    char buffer[KU_SERVICE_OUTPUT_RECORD_BYTES];
    int count = vsnprintf(buffer, sizeof(buffer), format, arguments);
    if (count < 0) {
      status = -1;
      ku_output_lock();
      if (ku_service_output.metrics.failures != SIZE_MAX) ku_service_output.metrics.failures++;
      ku_output_unlock();
    } else if ((size_t)count >= sizeof(buffer)) {
      ku_output_lock(); ku_output_drop_locked((size_t)count); ku_output_unlock();
    } else ku_output_write(buffer, (size_t)count);
  } else
#endif
  status = vfprintf(stdout, format, arguments) < 0 ? -1 : 0;
  va_end(arguments);
  return status;
}

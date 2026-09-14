//! Native external root for the bounded source Task subset.

use super::output::COutput;
use crate::{
    error::KuResult,
    ir::task::{TaskFunctionId, TaskProgram},
};

pub(super) fn emit(out: &mut COutput, tasks: &TaskProgram, entry: TaskFunctionId) -> KuResult<()> {
    out.check()?;
    out.push_str(ROOT_PREFIX);
    for function in &tasks.functions {
        out.check()?;
        out.push_str(&format!(
            "  if (maximum_instance < sizeof(KuTaskInstance_{})) maximum_instance = sizeof(KuTaskInstance_{});\n",
            function.id.0, function.id.0,
        ));
    }
    out.push_str(&ROOT_BODY.replace("@ENTRY@", &entry.0.to_string()));
    out.check()
}

const ROOT_PREFIX: &str = r#"
#if !defined(_WIN32)
#include <unistd.h>
#endif
/* One external root, one bounded driver. The first source subset allocates no
 * dynamic value buffers: its finite byte limit covers fixed storage plus the
 * largest generated instance at each of the existing 1024 resident slots.
 * This is not an OS RSS limit or a budget for future I/O/allocator features.
 * Static storage survives a diagnosed quarantine until process termination. */
static KuTaskDriverV1 ku_task_root_driver;
static KuTaskDriverSlotV1 ku_task_root_slots[KU_TASK_DRIVER_MAX_SLOTS];
static size_t ku_task_root_ring[KU_TASK_DRIVER_MAX_SLOTS];
/* Query the execution target, never the compiler host. This bounded default
 * follows the current runtime policy; OS stacks are not charged Task bytes. */
static size_t ku_task_root_worker_count(void) {
  size_t count = 4;
#if defined(_WIN32)
  SYSTEM_INFO system_info;
  GetSystemInfo(&system_info);
  if (system_info.dwNumberOfProcessors) count = (size_t)system_info.dwNumberOfProcessors;
#else
  long available = sysconf(_SC_NPROCESSORS_ONLN);
  if (available > 0) count = (size_t)available;
#endif
  if (count < 4u) count = 4u;
  if (count > KU_TASK_DRIVER_MAX_WORKERS) count = KU_TASK_DRIVER_MAX_WORKERS;
  return count;
}
static uint64_t ku_task_root_deadline(uint64_t inherited) {
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) return 0;
  uint64_t fresh = now > UINT64_MAX - 1001u ? UINT64_MAX - 1u : now + 1000u;
  return ku_task_driver_min(inherited, fresh);
}
/* Keep the original error while still reaping a fully drained faulty group.
 * A retained LIVE/REAPING object remains in static storage on failed cleanup. */
static int ku_task_root_finish(uint64_t deadline) {
  uint32_t shutdown = KU_TASK_DRIVER_OK;
  if (ku_task_root_driver.initialized == KU_TASK_DRIVER_STORAGE_LIVE)
    shutdown = ku_task_driver_shutdown(&ku_task_root_driver, deadline);
  uint32_t joined = ku_task_driver_join(&ku_task_root_driver, deadline);
  uint32_t destroyed = joined == KU_TASK_DRIVER_OK
      ? ku_task_driver_destroy(&ku_task_root_driver) : KU_TASK_DRIVER_PENDING;
  if (shutdown != KU_TASK_DRIVER_OK) {
    fputs(shutdown == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT
        ? "native Task cleanup did not finish within its original deadline\n"
        : "native Task cleanup reported a runtime error\n", stderr);
  } else if (joined != KU_TASK_DRIVER_OK) {
    fputs("native Task workers could not be joined within their original deadline\n", stderr);
  } else if (destroyed != KU_TASK_DRIVER_OK) {
    fputs("native Task runtime could not be destroyed\n", stderr);
  }
  return shutdown != KU_TASK_DRIVER_OK || joined != KU_TASK_DRIVER_OK
      || destroyed != KU_TASK_DRIVER_OK;
}
int main(void) {
  size_t maximum_instance = 0;
"#;

const ROOT_BODY: &str = r#"
  size_t fixed = sizeof(ku_task_root_driver) + sizeof(ku_task_root_slots) + sizeof(ku_task_root_ring);
  if (maximum_instance > (SIZE_MAX - fixed) / KU_TASK_DRIVER_MAX_SLOTS) {
    fputs("native Task storage exceeds its bounded layout\n", stderr); return 1;
  }
  uint64_t startup_deadline = ku_task_root_deadline(UINT64_MAX);
  uint32_t status = ku_task_driver_init(&ku_task_root_driver, sizeof(ku_task_root_driver),
      KU_TASK_DRIVER_ABI_VERSION, ku_task_root_slots, KU_TASK_DRIVER_MAX_SLOTS,
      ku_task_root_ring, KU_TASK_DRIVER_MAX_SLOTS, fixed + maximum_instance * KU_TASK_DRIVER_MAX_SLOTS,
      ku_task_root_worker_count(), startup_deadline);
  if (status != KU_TASK_DRIVER_OK) {
    fputs("native Task runtime initialization failed\n", stderr);
    if (ku_task_root_driver.initialized != KU_TASK_DRIVER_STORAGE_ZERO)
      (void)ku_task_root_finish(startup_deadline);
    return 1;
  }
  KuTaskValueV1 root = {0};
  KuTaskAdapterOutcomeV1 outcome = {0};
  uint64_t inherited_deadline = UINT64_MAX;
  int exit_code = 0;
  status = ku_task_@ENTRY@_start_value(&ku_task_root_driver, &root);
  if (status == KU_TASK_DRIVER_OK && root.tag == KU_TASK_VALUE_LIVE)
    status = ku_task_driver_wait_result(&root.ticket, &root.owner.lease, UINT64_MAX);
  if (status == KU_TASK_DRIVER_OK || status == KU_TASK_DRIVER_WAIT_READY)
    status = ku_task_value_take(&root, NULL, &outcome);
  if (status == KU_TASK_CONTROL_OK) {
    if (outcome.result_kind != 3u) {
      fputs("native Task entry returned an invalid payload\n", stderr); exit_code = 1;
    } else if (!outcome.value.null_value.ok) {
      /* Diagnostic I/O failure cannot call the synchronous exiting helper and
       * bypass Result drop or the root's original cleanup budget. */
      KuString message = outcome.value.null_value.error.message;
      if (message.len) (void)fwrite(message.ptr, 1, message.len, stderr);
      fputc('\n', stderr); exit_code = 1;
    }
    if (outcome.has_cleanup_deadline) inherited_deadline = outcome.cleanup_deadline;
  } else {
    exit_code = 1;
    if ((status == KU_TASK_CONTROL_CANCELLED || status == KU_TASK_CONTROL_TIMED_OUT)
        && root.tag == KU_TASK_VALUE_LIVE) {
      inherited_deadline = ku_task_control_cleanup_deadline(root.owner.lease.control);
      fputs(status == KU_TASK_CONTROL_TIMED_OUT ? "task timed out\n" : "task cancelled\n", stderr);
    } else fputs("native Task runtime failed\n", stderr);
  }
  /* Root retains the actual resident charge while inspecting its heap result;
   * its Result is dropped before the owner can return that reservation. */
  ku_task_outcome_drop(&outcome);
  uint64_t deadline = ku_task_root_deadline(inherited_deadline);
  if (root.tag != KU_TASK_VALUE_EMPTY && ku_task_value_drop(&root, deadline) != KU_TASK_DRIVER_OK) {
    fputs("native Task root ownership could not be released\n", stderr); exit_code = 1;
  }
  if (ku_task_root_finish(deadline)) exit_code = 1;
  return exit_code;
}
"#;

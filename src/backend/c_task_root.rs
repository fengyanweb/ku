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
/* One external root, one bounded driver. The first source subset allocates no
 * dynamic value buffers: its finite byte limit covers fixed storage plus the
 * largest generated instance at each of the existing 1024 resident slots.
 * This is not an OS RSS limit or a budget for future I/O/allocator features.
 * Static storage survives a diagnosed quarantine until process termination. */
static KuTaskDriverV1 ku_task_root_driver;
static KuTaskDriverSlotV1 ku_task_root_slots[KU_TASK_DRIVER_MAX_SLOTS];
static size_t ku_task_root_ring[KU_TASK_DRIVER_MAX_SLOTS];
static uint64_t ku_task_root_deadline(uint64_t inherited) {
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) return 0;
  uint64_t fresh = now > UINT64_MAX - 1001u ? UINT64_MAX - 1u : now + 1000u;
  return ku_task_driver_min(inherited, fresh);
}
int main(void) {
  size_t maximum_instance = 0;
"#;

const ROOT_BODY: &str = r#"
  size_t fixed = sizeof(ku_task_root_driver) + sizeof(ku_task_root_slots) + sizeof(ku_task_root_ring);
  if (maximum_instance > (SIZE_MAX - fixed) / KU_TASK_DRIVER_MAX_SLOTS) {
    fputs("native Task storage exceeds its bounded layout\n", stderr); return 1;
  }
  uint32_t status = ku_task_driver_init(&ku_task_root_driver, sizeof(ku_task_root_driver),
      KU_TASK_DRIVER_ABI_VERSION, ku_task_root_slots, KU_TASK_DRIVER_MAX_SLOTS,
      ku_task_root_ring, KU_TASK_DRIVER_MAX_SLOTS, fixed + maximum_instance * KU_TASK_DRIVER_MAX_SLOTS);
  if (status != KU_TASK_DRIVER_OK) { fputs("native Task runtime initialization failed\n", stderr); return 1; }
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
  uint32_t shutdown = ku_task_driver_shutdown(&ku_task_root_driver, deadline);
  if (shutdown != KU_TASK_DRIVER_OK) {
    fputs("native Task cleanup did not finish within its original deadline\n", stderr);
    return 1; /* quarantined storage is NOT claimed as drained or freed */
  }
#if defined(_WIN32)
  /* worker_exited precedes the last OS thread-return instruction. Wait on that
   * handle once under the remaining original budget; no Pending/yield loop. */
  uint64_t now = ku_task_driver_now_ms();
  uint64_t remaining = now != UINT64_MAX && now < deadline ? deadline - now : 0;
  DWORD timeout = remaining >= (uint64_t)INFINITE ? INFINITE - 1u : (DWORD)remaining;
  if (WaitForSingleObject(ku_task_root_driver.thread, timeout) != WAIT_OBJECT_0) {
    fputs("native Task worker did not stop within its original deadline\n", stderr); return 1;
  }
#endif
  if (ku_task_driver_destroy(&ku_task_root_driver) != KU_TASK_DRIVER_OK) {
    fputs("native Task runtime could not be destroyed\n", stderr); return 1;
  }
  return exit_code;
}
"#;

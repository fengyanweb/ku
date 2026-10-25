//! Native external root for the bounded source Task subset.

use super::output::COutput;
use crate::{
    error::KuResult,
    ir::task::{TaskFunctionId, TaskProgram},
};

pub(super) fn emit(out: &mut COutput, tasks: &TaskProgram, entry: TaskFunctionId) -> KuResult<()> {
    out.check()?;
    let blocking = super::task::uses_blocking_io(tasks);
    let network = super::task::uses_blocking_net(tasks);
    out.push_str(
        &ROOT_PREFIX
            .replace("@NETWORK_STORAGE@",if network {"static KuTaskNetRootV1 ku_task_root_net;"} else {""})
            .replace("@NETWORK_REFUND@",if network {NETWORK_REFUND} else {""})
            .replace("@NETWORK_FINISH@",if network {"  if(destroyed==KU_TASK_DRIVER_OK)ku_task_net_root_destroy(&ku_task_root_net);"} else {""})
            .replace(
                "@BLOCKING_STORAGE@",
                if blocking {
                    "static KuTaskBlockingPoolV1 ku_task_root_blocking;"
                } else {
                    ""
                },
            )
            .replace("@BLOCKING_DIAGNOSTIC@", if blocking {
                "    fputs(ku_task_root_blocking.initialized==KU_TASK_DRIVER_STORAGE_ZERO ? \"native blocking pool reported a runtime error after resource cleanup\\n\" : blocking==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT ? \"native blocking jobs have not physically retired within the original deadline\\n\" : \"native blocking pool resources could not be fully reclaimed\\n\",stderr);"
            } else { "" })
            .replace(
                "@BLOCKING_FINISH@",
                if blocking {
                    "  uint32_t blocking=ku_task_blocking_finish(&ku_task_root_blocking,deadline);"
                } else {
                    "  uint32_t blocking=KU_TASK_DRIVER_OK;"
                },
            ),
    );
    for function in &tasks.functions {
        out.check()?;
        out.push_str(&format!(
            "  if (maximum_instance < sizeof(KuTaskInstance_{})) maximum_instance = sizeof(KuTaskInstance_{});\n",
            function.id.0, function.id.0,
        ));
    }
    out.push_str(
        &ROOT_BODY
            .replace("@NETWORK_FIXED@",if network {" + ku_task_net_root_fixed_bytes(ku_task_root_worker_count(),KU_TASK_ROOT_CAPACITY)"} else {""})
            .replace("@NETWORK_INIT@",if network {"  status=ku_task_driver_scratch_reserve(&ku_task_root_driver,sizeof(ku_task_root_net));\n  if(status==KU_TASK_DRIVER_OK) {\n    status=ku_task_net_root_attach(&ku_task_root_net,&ku_task_root_driver,&ku_task_root_blocking);\n    if(status==KU_TASK_DRIVER_OK)ku_task_root_net.reserved=1;\n    else (void)ku_task_driver_scratch_refund(&ku_task_root_driver,sizeof(ku_task_root_net));\n  }\n  if(status!=KU_TASK_DRIVER_OK) { (void)ku_task_root_finish(startup_deadline); return 1; }"} else {""})
            .replace(
                "@BLOCKING_FIXED@",
                if blocking {
                    " + ku_task_blocking_fixed_bytes(KU_TASK_ROOT_CAPACITY)"
                } else {
                    ""
                },
            )
            .replace("@BLOCKING_INIT@", if blocking { BLOCKING_INIT } else { "" })
            .replace("@ENTRY@", &entry.0.to_string()),
    );
    out.check()
}

const ROOT_PREFIX: &str = r#"
#if !defined(_WIN32)
#include <unistd.h>
#endif
/* One external root, one bounded driver. Its finite byte limit covers fixed
 * storage plus the largest generated instance at each of the existing 1024
 * resident slots. Owned inputs/projections/blocking copies share that finite
 * remainder; allocation rejects before a file operation when it is exhausted.
 * This is not an OS RSS limit, a libc FILE-buffer or an OS-stack budget.
 * Static storage survives a diagnosed quarantine until process termination. */
/* CLI defaults remain bounded at 1024; HTTP capacity is explicit. */
#define KU_TASK_ROOT_CAPACITY ((size_t)1024u)
static KuTaskDriverV1 ku_task_root_driver;
static KuTaskDriverSlotV1 ku_task_root_slots[KU_TASK_ROOT_CAPACITY];
static size_t ku_task_root_ring[KU_TASK_ROOT_CAPACITY];
@BLOCKING_STORAGE@
@NETWORK_STORAGE@
/* Query the execution target, never the compiler host. This bounded default
 * follows the current runtime policy; OS stacks are not charged Task bytes. */
static size_t ku_task_root_worker_count(void) {
  return ku_runtime_loop_count();
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
@BLOCKING_FINISH@
  uint32_t joined = KU_TASK_DRIVER_OK;
@NETWORK_REFUND@
  if(joined==KU_TASK_DRIVER_OK)joined=ku_task_driver_join(&ku_task_root_driver, deadline);
  uint32_t destroyed = joined == KU_TASK_DRIVER_OK
      ? ku_task_driver_destroy(&ku_task_root_driver) : KU_TASK_DRIVER_PENDING;
@NETWORK_FINISH@
  if (shutdown != KU_TASK_DRIVER_OK) {
    fputs(shutdown == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT
        ? "native Task cleanup did not finish within its original deadline\n"
        : "native Task cleanup reported a runtime error\n", stderr);
  } else if (blocking != KU_TASK_DRIVER_OK) {
@BLOCKING_DIAGNOSTIC@
  } else if (joined != KU_TASK_DRIVER_OK) {
    fputs("native Task workers could not be joined within their original deadline\n", stderr);
  } else if (destroyed != KU_TASK_DRIVER_OK) {
    fputs("native Task runtime could not be destroyed\n", stderr);
  }
  return shutdown != KU_TASK_DRIVER_OK || blocking != KU_TASK_DRIVER_OK || joined != KU_TASK_DRIVER_OK
      || destroyed != KU_TASK_DRIVER_OK;
}
int main(void) {
  size_t maximum_instance = 0;
"#;

const NETWORK_REFUND: &str = r#"
  if(ku_task_root_net.reserved) {
    if(ku_task_driver_lock(&ku_task_root_driver)) joined=KU_TASK_DRIVER_INTERNAL;
    else {
      int retired=!ku_task_root_driver.resident && !ku_task_root_driver.running
          && !ku_task_root_driver.queued && ku_task_root_driver.workers_exited==ku_task_root_driver.workers_created;
      ku_task_driver_unlock(&ku_task_root_driver);
      if(!retired)joined=KU_TASK_DRIVER_PENDING;
      else {
        /* EXITED is every worker's last storage access. Close the actual
         * channels before refunding; reaping itself requires zero receipts. */
        joined=ku_task_net_root_destroy(&ku_task_root_net);
        if(joined==KU_TASK_DRIVER_OK)joined=ku_task_driver_scratch_refund(&ku_task_root_driver,sizeof(ku_task_root_net));
        if(joined==KU_TASK_DRIVER_OK)ku_task_root_net.reserved=0;
      }
    }
  }
"#;

const ROOT_BODY: &str = r#"
  size_t driver_fixed = ku_task_driver_fixed_storage_bytes(KU_TASK_ROOT_CAPACITY);
  if (!driver_fixed) {
    fputs("native Task storage exceeds its bounded layout\n", stderr); return 1;
  }
  size_t fixed = driver_fixed@BLOCKING_FIXED@@NETWORK_FIXED@;
  if (maximum_instance > (SIZE_MAX - fixed) / KU_TASK_ROOT_CAPACITY) {
    fputs("native Task storage exceeds its bounded layout\n", stderr); return 1;
  }
  uint64_t startup_deadline = ku_task_root_deadline(UINT64_MAX);
  uint32_t status = ku_task_driver_init(&ku_task_root_driver, sizeof(ku_task_root_driver),
      KU_TASK_DRIVER_ABI_VERSION, ku_task_root_slots, KU_TASK_ROOT_CAPACITY,
      ku_task_root_ring, KU_TASK_ROOT_CAPACITY, fixed + maximum_instance * KU_TASK_ROOT_CAPACITY,
      ku_task_root_worker_count(), startup_deadline);
  if (status != KU_TASK_DRIVER_OK) {
    fputs("native Task runtime initialization failed\n", stderr);
    if (ku_task_root_driver.initialized != KU_TASK_DRIVER_STORAGE_ZERO)
      (void)ku_task_root_finish(startup_deadline);
    return 1;
  }
  status=ku_task_driver_set_fixed_owner(&ku_task_root_driver);
  if (status!=KU_TASK_DRIVER_OK) {
    fputs("native Task owner initialization failed\n",stderr);
    (void)ku_task_root_finish(startup_deadline); return 1;
  }
@BLOCKING_INIT@
@NETWORK_INIT@
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

const BLOCKING_INIT: &str = r#"
  status=ku_task_blocking_init(&ku_task_root_blocking,KU_TASK_ROOT_CAPACITY,
      ku_runtime_blocking_count(),startup_deadline);
  if (status==KU_TASK_DRIVER_OK)
    status=ku_task_driver_attach_blocking(&ku_task_root_driver,&ku_task_root_blocking,
        sizeof(ku_task_root_blocking),ku_task_blocking_close,ku_task_blocking_ready,
        ku_task_root_blocking.storage,ku_task_root_blocking.storage_bytes);
  if (status!=KU_TASK_DRIVER_OK) {
    fputs("native blocking pool initialization failed\n",stderr);
    (void)ku_task_root_finish(startup_deadline);
    return 1;
  }
"#;

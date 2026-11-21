#define CHECK(c) do { if (!(c)) { fprintf(stderr, "enum outcome line %d: %s\n", __LINE__, #c); return 1; } } while (0)

static void test_wake(void* data, size_t owner) { (void)data; (void)owner; }

static int trial(int64_t input, bool cancel) {
  KuTaskDriverV1 driver={0};
  KuTaskDriverSlotV1 slots[2]={0};
  size_t ring[2]={0};
  KuTaskValueV1 task={0};
  KuTaskAdapterOutcomeV1 output={0};
  uint64_t deadline=3000, next=UINT64_MAX;
  size_t fixed=ku_task_driver_fixed_storage_bytes(2u);
  bool failed=input==-2;
  fixture_now=1000;
  fixture_rejected_tags=0;
  CHECK(ku_task_driver_init_external(&driver, sizeof(driver), KU_TASK_DRIVER_ABI_VERSION,
      slots, 2u, ring, 2u, fixed+sizeof(KuTaskInstance_0)+4096u, 1u, deadline, NULL, test_wake)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_0_start_value(&driver, &input, &task)==KU_TASK_DRIVER_OK);
  for (size_t i=0; i<64u && !driver.timers; ++i)
    CHECK(ku_task_driver_pump(&driver, 0u, 1u, &next)<=KU_TASK_DRIVER_PENDING);
  CHECK(driver.timers==1u && driver.resident==1u && driver.reserved_bytes>0u);
  CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_value_take(&task, NULL, &output)!=KU_TASK_CONTROL_OK && !output.result_kind);
  if (cancel) {
    CHECK(ku_task_driver_request_cancel(&task.ticket, &task.owner.lease, KU_TASK_CONTROL_CANCELLED, 1300u)==KU_TASK_CONTROL_OK);
    CHECK(ku_task_control_cleanup_deadline(task.owner.lease.control)==1300u);
  }
  fixture_now=1031;
  for (size_t i=0; i<64u && ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING; ++i)
    CHECK(ku_task_driver_pump(&driver, 0u, 1u, &next)<=KU_TASK_DRIVER_PENDING);
  if (cancel) {
    CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_CANCELLED);
    CHECK(!fixture_rejected_tags);
    CHECK(ku_task_value_take(&task, NULL, &output)!=KU_TASK_CONTROL_OK && !output.result_kind);
  } else {
    CHECK(ku_task_control_status(&task.owner.lease)==(failed ? KU_TASK_CONTROL_FAILED : KU_TASK_CONTROL_COMPLETED));
    CHECK(fixture_rejected_tags==(failed ? 0u : 1u));
    /* Tag zero is Found, so a nonempty destination has an active int field. */
    output.value.enum_ProbeStatus.value.payload.Found.index=123;
    CHECK(ku_task_value_take(&task, NULL, &output)!=KU_TASK_CONTROL_OK && !output.result_kind);
    output.value.enum_ProbeStatus.value.payload.Found.index=0;
    CHECK(ku_task_value_take(&task, NULL, &output)==KU_TASK_CONTROL_OK);
    CHECK(output.result_kind==KU_TASK_OUTCOME_KIND_ENUM_ProbeStatus && output.exit_class==KU_TASK_EXIT_USER_RESULT);
    KuTaskAdapterOutcomeV1 second={0};
    CHECK(ku_task_value_take(&task, NULL, &second)!=KU_TASK_CONTROL_OK && !second.result_kind);
  }
  if (task.tag) CHECK(ku_task_value_drop(&task, deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver, deadline)==KU_TASK_DRIVER_OK);
  for (size_t i=0; i<64u && driver.workers_exited!=1u; ++i)
    CHECK(ku_task_driver_pump(&driver, 0u, 16u, &next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers && !driver.reserved_bytes && !driver.fault);
  CHECK(ku_task_driver_join(&driver, deadline)==KU_TASK_DRIVER_OK && ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  if (!cancel && failed) {
    CHECK(!output.value.enum_ProbeStatus.ok);
    KuString message=output.value.enum_ProbeStatus.error.message;
    CHECK(message.len==11u && !memcmp(message.ptr, "read failed", 11u));
  } else if (!cancel) {
    CHECK(output.value.enum_ProbeStatus.ok);
    CHECK(output.value.enum_ProbeStatus.value.tag==(input<0 ? 1 : 0));
    if (input>=0) CHECK(output.value.enum_ProbeStatus.value.payload.Found.index==input);
  }
  ku_task_outcome_drop(&output);
  CHECK(ku_perf_calls && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  return 0;
}

int main(void) {
  CHECK(!trial(42, false));
  CHECK(!trial(INT64_MAX, false));
  CHECK(!trial(0, false));
  CHECK(!trial(-1, false));
  CHECK(!trial(-2, false));
  CHECK(!trial(42, true));
  puts("int enum pending, validated take, cancellation and retirement ok");
  return 0;
}

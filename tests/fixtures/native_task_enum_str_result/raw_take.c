#define CHECK(c) do { if (!(c)) { fprintf(stderr, "owned enum outcome line %d: %s\n", __LINE__, #c); return 1; } } while (0)

static void test_wake(void* data, size_t owner) { (void)data; (void)owner; }

enum Disposition { UNPACK, DROP_OUTCOME, CANCEL, UNRECEIVED, INLINE_FAILURE };

static int trial(int64_t mode, enum Disposition disposition) {
  bool cancel=disposition==CANCEL, discard=disposition==UNRECEIVED;
  KuTaskDriverV1 driver={0};
  KuTaskDriverSlotV1 slots[2]={0};
  size_t ring[2]={0};
  KuTaskValueV1 task={0};
  KuTaskAdapterOutcomeV1 output={0};
  uint64_t deadline=3000, next=UINT64_MAX;
  size_t fixed=ku_task_driver_fixed_storage_bytes(2u);
  fixture_now=1000;
  fixture_rejected_tags=0;
  fixture_rejected_ranges=0;
  KuString label=ku_string_alloc(8u);
  CHECK(label.ptr);
  memcpy(label.ptr,"rejected",8u);
  uint8_t* original=label.ptr;
  CHECK(ku_task_driver_init_external(&driver, sizeof(driver), KU_TASK_DRIVER_ABI_VERSION,
      slots, 2u, ring, 2u, fixed+sizeof(KuTaskInstance_0)+64u, 1u, deadline, NULL, test_wake)==KU_TASK_DRIVER_OK);
  if (disposition==INLINE_FAILURE) driver.byte_limit=fixed;
  CHECK(ku_task_0_start_value(&driver,&label,&mode,&task)==KU_TASK_DRIVER_OK);
  CHECK(!label.ptr && !label.len);
  if (disposition==INLINE_FAILURE) {
    CHECK(task.tag==KU_TASK_VALUE_INLINE_FAILED && !driver.resident && !driver.reserved_bytes);
    CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_DRIVER_OK);
    CHECK(output.result_kind==KU_TASK_OUTCOME_KIND_ENUM_ProbeReport);
    CHECK(!output.value.enum_ProbeReport.ok && !output.value.enum_ProbeReport.value.ptr);
    KuString code=output.value.enum_ProbeReport.error.code;
    CHECK(code.len==14u && !memcmp(code.ptr,"too_many_tasks",14u));
  } else {
    for (size_t i=0; i<64u && !driver.timers; ++i)
      CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
    CHECK(driver.timers==1u && driver.resident==1u && driver.reserved_bytes==sizeof(KuTaskInstance_0)+8u);
    CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING);
    CHECK(ku_task_value_take(&task,NULL,&output)!=KU_TASK_CONTROL_OK && !output.result_kind);
    if (cancel) {
      CHECK(ku_task_driver_request_cancel(&task.ticket,&task.owner.lease,KU_TASK_CONTROL_CANCELLED,1300u)==KU_TASK_CONTROL_OK);
      CHECK(ku_task_control_cleanup_deadline(task.owner.lease.control)==1300u);
    }
    fixture_now=1031;
    for (size_t i=0; i<64u && ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING; ++i)
      CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
    if (cancel) {
      CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_CANCELLED);
      CHECK(!fixture_rejected_tags && !fixture_rejected_ranges);
      CHECK(ku_task_value_take(&task,NULL,&output)!=KU_TASK_CONTROL_OK && !output.result_kind);
    } else {
      CHECK(ku_task_control_status(&task.owner.lease)==(mode==2 ? KU_TASK_CONTROL_FAILED : KU_TASK_CONTROL_COMPLETED));
      CHECK(fixture_rejected_tags==(mode==2 ? 0u : 1u));
      CHECK(fixture_rejected_ranges==(mode==1 ? 0u : 1u));
      CHECK(driver.reserved_bytes==sizeof(KuTaskInstance_0)+(mode==1 ? 0u : 8u));
      if (!discard) {
        if (mode!=1) {
          /* The owned allocation is smaller than either raw destination. */
          CHECK(ku_task_control_take_result(&task.owner.lease,original)==KU_TASK_CONTROL_INVALID_ARGUMENT);
          CHECK(ku_task_value_take(&task,NULL,(KuTaskAdapterOutcomeV1*)(void*)original)==KU_TASK_CONTROL_INVALID_ARGUMENT);
        }
        output.value.enum_ProbeReport.value=ku_string_static((const uint8_t*)"occupied",8u);
        CHECK(ku_task_value_take(&task,NULL,&output)!=KU_TASK_CONTROL_OK && !output.result_kind);
        output.value.enum_ProbeReport.value=(KuString){0};
        size_t allocations=ku_perf_calls;
        CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
        CHECK(ku_perf_calls==allocations);
        CHECK(output.result_kind==KU_TASK_OUTCOME_KIND_ENUM_ProbeReport && output.exit_class==KU_TASK_EXIT_USER_RESULT);
        KuTaskAdapterOutcomeV1 second={0};
        CHECK(ku_task_value_take(&task,NULL,&second)!=KU_TASK_CONTROL_OK && !second.result_kind);
      }
    }
  }
  if (task.tag) CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for (size_t i=0; i<64u && driver.workers_exited!=1u; ++i)
    CHECK(ku_task_driver_pump(&driver,0u,16u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers && !driver.reserved_bytes && !driver.fault);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK && ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  if (disposition==UNPACK) {
    size_t allocations=ku_perf_calls;
    KuResult_enum_ProbeReport result=ku_task_unpack_enum_ProbeReport(&output.value.enum_ProbeReport);
    CHECK(ku_perf_calls==allocations && !output.value.enum_ProbeReport.ok && !output.value.enum_ProbeReport.value.ptr);
    if (mode==2) {
      CHECK(!result.ok && result.error.message.ptr==original && result.error.message.len==8u);
    } else if (mode==1) {
      CHECK(result.ok && result.value.tag==1);
    } else {
      CHECK(result.ok && result.value.tag==0);
      CHECK(result.value.payload.Rejected.label.ptr==original && result.value.payload.Rejected.label.len==8u);
    }
    if (mode!=1) CHECK(!memcmp(original,"rejected",8u));
    ku_result_drop_enum_ProbeReport(&result);
  } else if (disposition==DROP_OUTCOME) {
    CHECK((mode==2 ? output.value.enum_ProbeReport.error.message.ptr : output.value.enum_ProbeReport.value.ptr)==original);
  }
  ku_task_outcome_drop(&output);
  CHECK(ku_perf_calls && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  return 0;
}

int main(void) {
  CHECK(!trial(0,UNPACK));
  CHECK(!trial(1,UNPACK));
  CHECK(!trial(2,UNPACK));
  CHECK(!trial(0,DROP_OUTCOME));
  CHECK(!trial(2,DROP_OUTCOME));
  CHECK(!trial(0,CANCEL));
  CHECK(!trial(0,UNRECEIVED));
  CHECK(!trial(2,UNRECEIVED));
  CHECK(!trial(0,INLINE_FAILURE));
  puts("owned enum pending, alias guards, pointer transfer, and drain ok");
  return 0;
}

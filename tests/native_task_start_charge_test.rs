//! Source-lowered multihop Owned transfer with real observer leases.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
#[path = "support/native_task_ledger.rs"]
mod native_task_ledger;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use native_task_ledger::{LEDGER_LOCK, LOCKED_ALLOCATIONS};
use std::{fs, process::Command};

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "charge hook: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_start_charge_multihop_active_results_and_late_observers_execute_in_c() {
    for (index, parameter, result, leaf, input_c, output_field, modes) in [
        (
            0,
            "str",
            "str!",
            "return ok(value)",
            "KuString",
            "string",
            3,
        ),
        (
            1,
            "str!",
            "str!",
            "return value",
            "KuResult_str",
            "string",
            2,
        ),
        (
            2,
            "int!",
            "int!",
            "return value",
            "KuResult_int",
            "integer",
            2,
        ),
        (
            3,
            "[int]",
            "int!",
            "return ok(value.len())",
            "KuArray_int",
            "integer",
            2,
        ),
        (
            4,
            "[bool]",
            "int!",
            "return ok(value.len())",
            "KuArray_bool",
            "integer",
            2,
        ),
        (
            5,
            "[str]",
            "int!",
            "return ok(value.len())",
            "KuArray_str",
            "integer",
            3,
        ),
        (
            6,
            "[int]",
            "[int]!",
            "return ok(value)",
            "KuArray_int",
            "integer_array",
            2,
        ),
        (
            7,
            "[bool]",
            "[bool]!",
            "return ok(value)",
            "KuArray_bool",
            "boolean_array",
            2,
        ),
        (
            8,
            "[str]",
            "[str]!",
            "return ok(value)",
            "KuArray_str",
            "string_array",
            3,
        ),
        (
            9,
            "[str]!",
            "[str]!",
            "return value",
            "KuResult_array_str",
            "string_array",
            2,
        ),
    ] {
        let parent_return = if index >= 6 {
            "return ok((await child)?)"
        } else {
            "return await child"
        };
        let child_start = if index == 9 {
            "target: async fn([str]!): [str]! = Child; child=target(value)"
        } else {
            "child=Child(value)"
        };
        let source = format!("async fn Parent(value: {parameter}): {result} {{ {child_start} {parent_return} }}\nasync fn Child(value: {parameter}): {result} {{ child = Grandchild(value) return await child }}\nasync fn Grandchild(value: {parameter}): {result} {{ {leaf} }}\nasync fn main(): null! {{ return ok(null) }}");
        let ast = Parser::new(Lexer::new(&source).lex().unwrap())
            .parse_program()
            .unwrap();
        Checker::new().check(&ast).unwrap();
        let tasks = task_lower::lower_program(&ast).unwrap();
        for (i, name) in ["Parent", "Child", "Grandchild", "main"].iter().enumerate() {
            assert_eq!(tasks.tasks.functions[i].name, *name);
        }
        let generated = c::generate_native_task_c_source(&tasks, &Default::default()).unwrap();
        assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
        let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n");
        let generated = replace_once(
            generated,
            "typedef struct KuString {",
            &format!("{instrumentation}typedef struct KuString {{"),
        );
        // Observe the real commit/take calls. The wrappers never supply a
        // successful result or replace Start, Await, cleanup or accounting.
        let generated = replace_once(generated, "static uint32_t ku_task_driver_commit_child(", "static uint32_t ku_task_driver_commit_child(const KuTaskDriverTicketV1*, KuTaskControlOwnerV1*, const KuTaskDriverStartChargeV1*, size_t);\nstatic uint32_t fixture_real_commit_child(");
        let generated = replace_once(generated, "static uint32_t ku_task_driver_transfer_charge(", "static uint32_t ku_task_driver_transfer_charge(const KuTaskDriverTicketV1*, const KuTaskDriverTicketV1*, size_t, size_t);\nstatic uint32_t fixture_real_transfer_charge(");
        let mut generated = replace_once(
            generated,
            "int main(void) {",
            "static int fixture_unmodified_source_main(void) {",
        );
        generated.push_str(&format!("\ntypedef {input_c} FixtureInput;\n#define FIXTURE_KIND {index}\n#define FIXTURE_MODES {modes}\n"));
        if index >= 6 {
            let result_c = match index {
                6 => "KuResult_array_int",
                7 => "KuResult_array_bool",
                _ => "KuResult_array_str",
            };
            generated.push_str(&format!("typedef {result_c} FixtureResult;\n"));
        }
        generated.push_str(&C_MAIN.replace("@OUTPUT@", output_field));
        let directory = TempDir::new(&format!("native-task-start-multihop-{index}"));
        let path = directory.path().join("program.c");
        fs::write(&path, generated).unwrap();
        let Some(executable) = compile_harness(directory.path(), &path, "program") else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must execute multihop charge tests"
            );
            continue;
        };
        fs::remove_file(path).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .expect("multihop charge process is bounded");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            "task-start-multihop-ok\n"
        );
        assert!(output.stderr.is_empty());
    }
}

const C_MAIN: &str = r#"
static size_t fixture_expected_charge, fixture_buffers_count;
static uint8_t* fixture_buffers[3];
static KuTaskControlLeaseV1 fixture_observers[2];
static KuTaskDriverTicketV1 fixture_observer_tickets[2];
static size_t fixture_starts, fixture_takes;
static int fixture_mode,fixture_failure_probe;
#if FIXTURE_KIND==8
static void fixture_take_rejections(KuTaskControlV1*,const KuTaskDriverTicketV1*,size_t);
#endif
static uintptr_t fixture_vector;
static void fixture_sum(KuTaskDriverV1* driver) {
  size_t sum=0,resident=0;
  for (size_t i=0;i<driver->capacity;i++) {
    KuTaskDriverSlotV1* slot=&driver->slots[i];
    if (slot->state==KU_TASK_DRIVER_FREE) { CHECK(!slot->charged_bytes); continue; }
    CHECK(slot->charged_bytes<=SIZE_MAX-sum); sum+=slot->charged_bytes; resident++;
  }
  CHECK(sum==driver->reserved_bytes && resident==driver->resident);
}
static uint32_t ku_task_driver_commit_child(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,const KuTaskDriverStartChargeV1* source,size_t instance_bytes) {
  KuTaskDriverV1* driver=ticket->driver;
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* parent=ku_task_driver_find(&source->parent);
  KuTaskDriverSlotV1* child=ku_task_driver_find(ticket);
  CHECK(parent && child && fixture_starts<2u);
  CHECK(parent->state==KU_TASK_DRIVER_RUNNING && child->state==KU_TASK_DRIVER_BUILDING);
  CHECK(source->owned_bytes==fixture_expected_charge && child->charged_bytes==instance_bytes);
  CHECK(parent->charged_bytes==source->minimum_parent_bytes+fixture_expected_charge);
#if FIXTURE_KIND==9
  const KuResult_array_str* parameter=fixture_starts ? &((KuTaskInstance_2*)owner->lease.control)->frame.s_0
      : &((KuTaskInstance_1*)owner->lease.control)->frame.s_0;
  CHECK(parameter->ok==!fixture_mode);
  if (parameter->ok) CHECK((uintptr_t)parameter->value.data==fixture_vector);
  else CHECK(parameter->error.domain.ptr==fixture_buffers[0]&&parameter->error.code.ptr==fixture_buffers[1]
      &&parameter->error.message.ptr==fixture_buffers[2]);
#elif FIXTURE_KIND>=3
  CHECK((uintptr_t)(fixture_starts ? ((KuTaskInstance_2*)owner->lease.control)->frame.s_0.data
      : ((KuTaskInstance_1*)owner->lease.control)->frame.s_0.data)==fixture_vector);
#endif
  size_t total=driver->reserved_bytes;
  fixture_sum(driver); CHECK(!ku_task_driver_unlock(driver));
  uint32_t status=fixture_real_commit_child(ticket,owner,source,instance_bytes);
  CHECK(status==KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(driver));
  parent=ku_task_driver_find(&source->parent); child=ku_task_driver_find(ticket);
  CHECK(parent && child && parent->charged_bytes==source->minimum_parent_bytes);
  CHECK(child->charged_bytes==instance_bytes+fixture_expected_charge && driver->reserved_bytes==total);
  CHECK(ku_task_control_lease_retain(&owner->lease,&fixture_observers[fixture_starts])==KU_TASK_CONTROL_OK);
  fixture_observer_tickets[fixture_starts]=*ticket;
  fixture_starts++; fixture_sum(driver); CHECK(!ku_task_driver_unlock(driver));
  return status;
}
static uint32_t ku_task_driver_transfer_charge(const KuTaskDriverTicketV1* from,
    const KuTaskDriverTicketV1* to,size_t bytes,size_t minimum) {
  if (fixture_failure_probe) return fixture_real_transfer_charge(from,to,bytes,minimum);
  KuTaskDriverV1* driver=from->driver;
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* child=ku_task_driver_find(from);
  KuTaskDriverSlotV1* parent=ku_task_driver_find(to);
  CHECK(child && parent && fixture_takes<2u && bytes==(FIXTURE_KIND>=3 && FIXTURE_KIND<=5 ? 0u : fixture_expected_charge));
  CHECK(child->wrapper_active && ku_task_control_atomic_load(&child->binding->payload)==KU_TASK_CONTROL_PAYLOAD_TAKING);
  CHECK(child->charged_bytes==minimum+bytes);
  size_t prior_parent=parent->charged_bytes,total=driver->reserved_bytes;
  KuTaskControlV1* control=child->binding;
  fixture_sum(driver); CHECK(!ku_task_driver_unlock(driver));
#if FIXTURE_KIND==8
  if (!fixture_mode) fixture_take_rejections(control,to,minimum);
#endif
  uint32_t status=fixture_real_transfer_charge(from,to,bytes,minimum);
  CHECK(status==KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(driver));
  child=ku_task_driver_find(from); parent=ku_task_driver_find(to);
  CHECK(child && parent && child->charged_bytes==minimum);
  CHECK(parent->charged_bytes==prior_parent+bytes && driver->reserved_bytes==total);
  fixture_takes++; fixture_sum(driver); CHECK(!ku_task_driver_unlock(driver));
  return status;
}
#if FIXTURE_KIND==8
static void fixture_take_rejections(KuTaskControlV1* control,const KuTaskDriverTicketV1* parent,size_t minimum) {
  void* raw=control->context;
  FixtureResult* payload=fixture_takes ? &((KuTaskInstance_1*)raw)->payload : &((KuTaskInstance_2*)raw)->payload;
  FixtureResult saved; memcpy(&saved,payload,sizeof(saved));
  CHECK(saved.ok && saved.value.data && saved.value.capacity==4u);
  unsigned char vector[4u*sizeof(*saved.value.data)]; memcpy(vector,saved.value.data,sizeof(vector));
  FixtureLedger before=fixture_ledger();
  KuTaskDriverV1* driver=parent->driver;
  const KuTaskDriverTicketV1* child_ticket=fixture_takes ? &((KuTaskInstance_1*)raw)->ticket : &((KuTaskInstance_2*)raw)->ticket;
  size_t references=ku_task_control_atomic_load(&control->references);
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* original_child=ku_task_driver_find(child_ticket);
  KuTaskDriverSlotV1* original_parent=ku_task_driver_find(parent); CHECK(original_child&&original_parent);
  size_t child_charge=original_child->charged_bytes,parent_charge_before=original_parent->charged_bytes;
  uint32_t owner_location=original_child->owner_location;
  CHECK(!ku_task_driver_unlock(driver));
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  unsigned rejected=0;
  for (unsigned variant=0;variant<9u;variant++) {
    KuTaskAdapterOutcomeV1 output={0};
    KuTaskAdapterTakeRequestV1 request={&output,NULL};
    void* destination=&request;
    switch (variant) {
      case 0: destination=saved.value.data; break;
      case 1: request.outcome=(KuTaskAdapterOutcomeV1*)saved.value.data; break;
      case 2: request.outcome=(KuTaskAdapterOutcomeV1*)(saved.value.data+saved.value.capacity-1u); break;
      case 3: request.parent=(const KuTaskDriverTicketV1*)saved.value.data; break;
      case 4: request.outcome=(KuTaskAdapterOutcomeV1*)payload; break;
      case 5: destination=raw; break;
      // These are genuine 1-byte allocations, never padded typed headers.
      case 6: destination=saved.value.data[1].ptr; break;
      case 7: request.outcome=(KuTaskAdapterOutcomeV1*)saved.value.data[1].ptr; break;
      case 8: request.parent=(const KuTaskDriverTicketV1*)saved.value.data[1].ptr; break;
    }
    KuTaskAdapterTakeRequestV1 saved_request; memcpy(&saved_request,&request,sizeof(request));
    CHECK(control->operations.take_payload(raw,destination)==KU_TASK_CONTROL_INVALID_ARGUMENT);
    CHECK(ku_task_frame_zero_bytes(&output,sizeof(output)) && !memcmp(payload,&saved,sizeof(saved)));
    CHECK(!memcmp(&request,&saved_request,sizeof(request)));
    CHECK(!memcmp(vector,saved.value.data,sizeof(vector)));
    CHECK(ku_task_control_atomic_load(&control->payload)==KU_TASK_CONTROL_PAYLOAD_TAKING);
    CHECK((fixture_takes ? ((KuTaskInstance_1*)raw)->payload_initialized : ((KuTaskInstance_2*)raw)->payload_initialized));
    FixtureLedger after=fixture_ledger(); CHECK(after.calls==before.calls&&after.allocations==before.allocations&&after.bytes==before.bytes);
    KuTaskDriverSnapshotV1 after_snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&after_snapshot)==KU_TASK_DRIVER_OK);
    CHECK(after_snapshot.reserved_bytes==snapshot.reserved_bytes && after_snapshot.resident==snapshot.resident && !after_snapshot.fault);
    CHECK(ku_task_control_atomic_load(&control->references)==references);
    CHECK(!ku_task_driver_lock(driver));
    CHECK(original_child->charged_bytes==child_charge&&original_parent->charged_bytes==parent_charge_before
        &&original_child->owner_location==owner_location&&original_child->wrapper_active);
    CHECK(!ku_task_driver_unlock(driver));
    rejected++;
  }
  CHECK(rejected==9u);
  // Fault injection is restricted to balanced charge metadata in this real
  // TAKING window. A global byte-limit cannot make a net-zero transfer fail.
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* child=ku_task_driver_find(fixture_takes ? &((KuTaskInstance_1*)raw)->ticket : &((KuTaskInstance_2*)raw)->ticket);
  KuTaskDriverSlotV1* target=ku_task_driver_find(parent); CHECK(child&&target&&minimum);
  size_t charge=child->charged_bytes,total=driver->reserved_bytes,parent_charge=target->charged_bytes;
  CHECK(charge>=minimum);
  child->charged_bytes=minimum-1u; driver->reserved_bytes-=charge-child->charged_bytes;
  fixture_sum(driver); CHECK(!ku_task_driver_unlock(driver));
  KuTaskAdapterOutcomeV1 output={0}; KuTaskAdapterTakeRequestV1 request={&output,parent};
  fixture_failure_probe=1;
  CHECK(control->operations.take_payload(raw,&request)==KU_TASK_DRIVER_LIMIT);
  fixture_failure_probe=0;
  CHECK(!memcmp(payload,&saved,sizeof(saved)) && ku_task_frame_zero_bytes(&output,sizeof(output)));
  CHECK(!memcmp(vector,saved.value.data,sizeof(vector)) && !saved.value.data[1].ptr[0]);
  CHECK(ku_task_control_atomic_load(&control->payload)==KU_TASK_CONTROL_PAYLOAD_TAKING);
  CHECK(!ku_task_driver_lock(driver));
  CHECK(child->charged_bytes==minimum-1u && target->charged_bytes==parent_charge && !driver->fault);
  child->charged_bytes=charge; driver->reserved_bytes=total;
  fixture_sum(driver); CHECK(!ku_task_driver_unlock(driver));
  FixtureLedger after=fixture_ledger(); CHECK(after.calls==before.calls&&after.allocations==before.allocations&&after.bytes==before.bytes);
  // The outer generated take now retries its original real transfer, with
  // the exact prior metadata restored. No wrapper fabricates success.
}
#endif
static KuString fixture_owned(size_t index,size_t capacity,const char* text) {
  size_t length=strlen(text),allocated=capacity ? capacity : 1u;
  CHECK(index<3u && length<=capacity);
  uint8_t* buffer=(uint8_t*)malloc(allocated); CHECK(buffer);
  if (length) memcpy(buffer,text,length); else buffer[0]=0;
  fixture_buffers[index]=buffer; fixture_buffers_count++;
  fixture_expected_charge+=allocated;
  return (KuString){buffer,length,capacity,KU_STRING_OWNED};
}
static KuString fixture_inactive(void) {
  /* Inactive Result bytes are deliberately not a valid active KuString. */
  return (KuString){(uint8_t*)(uintptr_t)1u,SIZE_MAX,SIZE_MAX,KU_STRING_OWNED};
}
static KuError fixture_error(int owned) {
  if (!owned) return (KuError){fixture_inactive(),fixture_inactive(),fixture_inactive()};
  return (KuError){fixture_owned(0,7u,"d"),fixture_owned(1,11u,"c"),fixture_owned(2,13u,"m")};
}
static FixtureInput fixture_input(int mode) {
  fixture_expected_charge=0; fixture_buffers_count=0;
  fixture_vector=0;
  memset(fixture_buffers,0,sizeof(fixture_buffers));
#if FIXTURE_KIND>=3
  FixtureInput input={0};
#if FIXTURE_KIND==9
  input.ok=mode==0; input.error=fixture_error(!input.ok);
  if (!input.ok) {
    input.value=(KuArray_str){SIZE_MAX,(KuString*)(uintptr_t)1u,SIZE_MAX};
    return input;
  }
  KuArray_str* array=&input.value;
#else
  if (mode==1) return input;
  FixtureInput* array=&input;
#endif
  array->capacity=4u; array->len=(FIXTURE_KIND==5 || FIXTURE_KIND==8 || FIXTURE_KIND==9) ? 3u : 2u;
  array->data=calloc(array->capacity,sizeof(*array->data)); CHECK(array->data);
  fixture_vector=(uintptr_t)array->data;
  fixture_buffers[0]=(uint8_t*)array->data; fixture_buffers_count=1u;
  fixture_expected_charge=array->capacity*sizeof(*array->data);
#if FIXTURE_KIND==3 || FIXTURE_KIND==6
  array->data[0]=17; array->data[1]=-9;
#elif FIXTURE_KIND==4 || FIXTURE_KIND==7
  array->data[0]=true; array->data[1]=false;
#else
  if (mode==2) {
    array->len=2u; array->data[0]=ku_string_static((const uint8_t*)"static",6u); array->data[1]=array->data[0];
  } else {
    array->data[0]=fixture_owned(1,31u,"payload");
    array->data[1]=fixture_owned(2,0u,"");
    array->data[2]=ku_string_static((const uint8_t*)"\xe4\xb8\xad",3u);
  }
#endif
  return input;
#elif FIXTURE_KIND==0
  if (mode==0) return fixture_owned(0,31u,"payload");
  if (mode==1) return fixture_owned(0,0u,"");
  return ku_string_static((const uint8_t*)"static",6u);
#else
  FixtureInput input={0}; input.ok=mode==0;
  input.error=fixture_error(!input.ok);
#if FIXTURE_KIND==1
  input.value=input.ok ? fixture_owned(0,31u,"payload") : fixture_inactive();
#else
  input.value=input.ok ? 42 : INT64_MIN;
#endif
  return input;
#endif
}
static void fixture_validate(KuTaskAdapterOutcomeV1* output,int mode) {
  CHECK(output->exit_class==KU_TASK_EXIT_USER_RESULT && !output->has_cleanup_deadline && !output->cleanup_deadline);
#if FIXTURE_KIND==9
  if (mode) {
    KuError error=output->value.string_array.error;
    CHECK(output->result_kind==7u&&!output->value.string_array.ok);
    CHECK(error.domain.ptr==fixture_buffers[0]&&error.code.ptr==fixture_buffers[1]&&error.message.ptr==fixture_buffers[2]);
    CHECK(error.domain.capacity==7u&&error.code.capacity==11u&&error.message.capacity==13u);
    CHECK(error.domain.ptr[0]=='d'&&error.code.ptr[0]=='c'&&error.message.ptr[0]=='m');
    return;
  }
#endif
#if FIXTURE_KIND>=6
  CHECK(output->result_kind==(FIXTURE_KIND==9 ? 7u : FIXTURE_KIND-1u) && output->value.@OUTPUT@.ok);
#elif FIXTURE_KIND==2 || FIXTURE_KIND>=3
  CHECK(output->result_kind==1u);
#else
  CHECK(output->result_kind==4u);
#endif
#if FIXTURE_KIND>=6
#if FIXTURE_KIND==9
  KuArray_str value=output->value.@OUTPUT@.value;
#else
  FixtureInput value=output->value.@OUTPUT@.value;
#endif
  CHECK(value.len==(mode==1 ? 0u : (FIXTURE_KIND==8 || FIXTURE_KIND==9) && mode==0 ? 3u : 2u));
  CHECK((uintptr_t)value.data==fixture_vector && value.capacity==(mode==1 ? 0u : 4u));
  if (mode!=1) {
#if FIXTURE_KIND==6
    CHECK(value.data[0]==17 && value.data[1]==-9);
#elif FIXTURE_KIND==7
    CHECK(value.data[0] && !value.data[1]);
#else
    if (mode==2) CHECK(value.data[0].storage==KU_STRING_STATIC && value.data[1].ptr==value.data[0].ptr);
    else {
      CHECK(value.data[0].ptr==fixture_buffers[1] && value.data[0].len==7u && value.data[0].capacity==31u);
      CHECK(!memcmp(value.data[0].ptr,"payload",7u));
      CHECK(value.data[1].ptr==fixture_buffers[2] && !value.data[1].len && !value.data[1].capacity && value.data[1].storage==KU_STRING_OWNED);
      CHECK(value.data[2].storage==KU_STRING_STATIC && value.data[2].len==3u && !memcmp(value.data[2].ptr,"\xe4\xb8\xad",3u));
    }
#endif
  }
#elif FIXTURE_KIND>=3
  CHECK(output->value.integer.ok && output->value.integer.value==(mode==1 ? 0 : FIXTURE_KIND==5 && mode==0 ? 3 : 2));
#elif FIXTURE_KIND==0
  CHECK(output->value.string.ok);
  KuString text=output->value.string.value;
  CHECK(text.len==(mode==0 ? 7u : mode==1 ? 0u : 6u));
  if (mode!=2) CHECK(text.ptr==fixture_buffers[0] && text.storage==KU_STRING_OWNED);
  else CHECK(text.storage==KU_STRING_STATIC && !memcmp(text.ptr,"static",6u));
  if (mode==0) CHECK(text.capacity==31u && !memcmp(text.ptr,"payload",7u));
  if (mode==1) CHECK(!text.capacity);
#else
  CHECK(output->value.@OUTPUT@.ok==(mode==0));
  if (mode) {
    KuError error=output->value.@OUTPUT@.error;
    CHECK(error.domain.ptr==fixture_buffers[0] && error.code.ptr==fixture_buffers[1] && error.message.ptr==fixture_buffers[2]);
    CHECK(error.domain.len==1u && error.code.len==1u && error.message.len==1u);
    CHECK(error.domain.capacity==7u && error.code.capacity==11u && error.message.capacity==13u);
    CHECK(error.domain.ptr[0]=='d' && error.code.ptr[0]=='c' && error.message.ptr[0]=='m');
  } else {
#if FIXTURE_KIND==1
    CHECK(output->value.string.value.ptr==fixture_buffers[0] && output->value.string.value.capacity==31u);
    CHECK(output->value.string.value.len==7u && !memcmp(output->value.string.value.ptr,"payload",7u));
#else
    CHECK(output->value.integer.value==42);
#endif
  }
#endif
}
static void fixture_case(int mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  FixtureInput input=fixture_input(mode);
  fixture_starts=0; fixture_takes=0; fixture_mode=mode; fixture_failure_probe=0;
  memset(fixture_observers,0,sizeof(fixture_observers));
  memset(fixture_observer_tickets,0,sizeof(fixture_observer_tickets));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(3,sizeof(*slots));
  size_t* ring=(size_t*)calloc(3,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=ku_task_driver_fixed_storage_bytes(3u);
  size_t instances=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+sizeof(KuTaskInstance_2);
  uint64_t startup_now=ku_task_driver_now_ms();
  CHECK(startup_now!=UINT64_MAX && startup_now<UINT64_MAX-1000u);
  uint64_t startup_deadline=startup_now+1000u;
  CHECK(ku_task_driver_init(driver,sizeof(*driver),6u,slots,3,ring,3,
      fixed+instances+fixture_expected_charge,1u,startup_deadline)==KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_frame_zero_bytes(driver,sizeof(*driver))
      && ku_task_frame_zero_bytes(slots,(3)*sizeof(*slots))
      && ku_task_frame_zero_bytes(ring,(3)*sizeof(*ring)));
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,3,ring,3,
      fixed+instances+fixture_expected_charge,1u,startup_deadline)==KU_TASK_DRIVER_OK);
  KuTaskValueV1 root={0};
  size_t before_calls=fixture_ledger().calls;
  CHECK(ku_task_0_start_value(driver,&input,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
#if FIXTURE_KIND==9
  CHECK(!input.ok&&!input.value.data&&!input.value.len&&!input.value.capacity);
  CHECK(!input.error.domain.ptr&&!input.error.code.ptr&&!input.error.message.ptr);
#elif FIXTURE_KIND>=3
  CHECK(!input.data && !input.len && !input.capacity);
#elif FIXTURE_KIND==0
  CHECK(!input.ptr && !input.len && !input.capacity && !input.storage);
#else
  CHECK(!input.ok);
  CHECK(!input.error.domain.ptr && !input.error.domain.len && !input.error.domain.capacity && !input.error.domain.storage);
  CHECK(!input.error.code.ptr && !input.error.code.len && !input.error.code.capacity && !input.error.code.storage);
  CHECK(!input.error.message.ptr && !input.error.message.len && !input.error.message.capacity && !input.error.message.storage);
#if FIXTURE_KIND==1
  CHECK(!input.value.ptr && !input.value.len && !input.value.capacity && !input.value.storage);
#else
  CHECK(!input.value);
#endif
#endif
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  CHECK(ku_task_driver_wait_result(&root.ticket,&root.owner.lease,deadline)==KU_TASK_DRIVER_WAIT_READY);
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(driver));
  fixture_sum(driver);
  CHECK(fixture_starts==2u && fixture_takes==2u && !driver->fault);
  const size_t remaining_charge=FIXTURE_KIND>=3 && FIXTURE_KIND<=5 ? 0u : fixture_expected_charge;
  CHECK(driver->resident==3u && driver->reserved_bytes==instances+remaining_charge);
  for (size_t i=0;i<2;i++) {
    KuTaskDriverSlotV1* child=ku_task_driver_find(&fixture_observer_tickets[i]);
    KuTaskControlV1* control=fixture_observers[i].control;
    CHECK(child && control && child->state==KU_TASK_DRIVER_RETIRING);
    CHECK(child->cleanup_acked_generation==child->generation && !child->wrapper_active);
    CHECK(child->owner_location==KU_TASK_DRIVER_OWNER_RELEASED);
    CHECK(!child->driver_lease.control && !child->execution_lease.control);
    CHECK(control->frame_destroyed && !ku_task_control_atomic_load(&control->lifecycle_pin));
    CHECK(child->charged_bytes==(i ? sizeof(KuTaskInstance_2) : sizeof(KuTaskInstance_1)));
  }
  CHECK(!ku_task_driver_unlock(driver));
  FixtureLedger ready=fixture_ledger();
  CHECK(ready.calls==before_calls+3u);
  CHECK(ready.allocations==7u+(FIXTURE_KIND>=3 && FIXTURE_KIND<=5 ? 0u : fixture_buffers_count)
      && ready.bytes==fixed+instances+remaining_charge);
  KuTaskAdapterOutcomeV1 output={0};
  CHECK(ku_task_value_take(&root,NULL,&output)==KU_TASK_CONTROL_OK);
  fixture_validate(&output,mode);
  CHECK(fixture_ledger().calls==ready.calls);
  ku_task_outcome_drop(&output);
  CHECK(fixture_ledger().allocations==7u && fixture_ledger().bytes==fixed+instances);
  uint64_t cleanup_now=ku_task_driver_now_ms();
  CHECK(cleanup_now!=UINT64_MAX && cleanup_now<UINT64_MAX-1000u);
  uint64_t cleanup_deadline=cleanup_now+1000u;
  CHECK(ku_task_value_drop(&root,cleanup_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(driver,cleanup_deadline)==KU_TASK_DRIVER_OK);
  for (size_t i=0;i<2;i++) CHECK(ku_task_control_lease_release(&fixture_observers[i])==KU_TASK_CONTROL_OK);
  CHECK(ku_task_driver_wait_idle(driver,cleanup_deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.resident && !snapshot.reserved_bytes);
  CHECK(ku_task_driver_shutdown(driver,cleanup_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_join(driver,cleanup_deadline)==KU_TASK_DRIVER_OK);
  CHECK(driver->worker_target==1u && driver->workers_created==1u && driver->workers_exited==1u
      && driver->workers_joined==1u && driver->workers[0].joined && driver->workers[0].closed);
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
int main(void) {
  for (int mode=0;mode<FIXTURE_MODES;mode++) fixture_case(mode);
  puts("task-start-multihop-ok"); return 0;
}
"#;

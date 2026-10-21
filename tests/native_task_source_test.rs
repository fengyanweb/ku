//! Real source -> verified Task IR -> generated factories/continuations/root.
//! No fixture callback supplies Start, Await or scope cleanup on its behalf.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_harness::{
    compile_harness, run_bounded, TempDir, BUILD_LIMITS, BUILD_TIMEOUT, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, process::Command};

fn checked(source: &str) -> ku::ast::Program {
    let ast = Parser::new(Lexer::new(source).lex().expect("source lexes"))
        .parse_program()
        .expect("source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    ast
}

fn native(source: &str) -> String {
    let tasks =
        task_lower::lower_program(&checked(source)).expect("source lowers to verified Task IR");
    c::generate_native_task_c_source(&tasks, &Default::default()).expect("native Task C generation")
}

#[test]
fn native_task_external_owner_pump_preserves_affinity_wakes_and_retirement() {
    let generated=native("async fn Ready(value: int): int! { return ok(value) } async fn main(): null! { return ok(null) } async fn Work(): int! { index=0; while(index<5000) {index=index+1} return ok(index) }");
    let generated = generated.replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    );
    let source = format!(
        "{generated}\n{}",
        r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"external Task owner line %d: %s\n",__LINE__,#c); return 1; } } while (0)
static size_t notifications[2];
static void owner_wake(void* context,size_t owner) { (void)context; if(owner<2u) notifications[owner]++; }
int main(void) {
  KuTaskDriverV1 driver={0};
  KuTaskDriverSlotV1 slots[4]={0}; size_t ring[4]={0};
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  uint64_t deadline=ku_task_driver_now_ms()+1000u;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,4u,ring,4u,fixed+4u*sizeof(KuTaskInstance_0),2u,deadline,NULL,owner_wake)==KU_TASK_DRIVER_OK);
  KuTaskValueV1 tasks[4]={0};
  for(size_t i=0;i<4u;i++) {
    int64_t value=(int64_t)i;
    CHECK(ku_task_0_start_value(&driver,&value,&tasks[i])==KU_TASK_DRIVER_OK);
    CHECK(slots[tasks[i].ticket.slot].owner_worker==i%2u+1u);
    for(size_t n=0;n<1000u;n++) CHECK(ku_task_driver_wake(&tasks[i].ticket)==KU_TASK_DRIVER_OK);
  }
  CHECK(driver.queued==4u && !driver.global_queued);
  CHECK(driver.workers[0].local_queued==2u && driver.workers[1].local_queued==2u);
  CHECK(notifications[0] && notifications[1]);
  uint64_t next=UINT64_MAX;
  size_t before_first_pump[2]={notifications[0],notifications[1]};
  CHECK(ku_task_driver_pump(&driver,0u,1u,&next)==KU_TASK_DRIVER_OK);
  CHECK(driver.workers[0].polls==1u && !driver.workers[1].polls);
  CHECK(next==0u && driver.queued==3u);
  /* Remaining ready work is already represented by next==0. The owner must
   * turn again without a socket self-notification under the driver mutex. */
  CHECK(notifications[0]==before_first_pump[0] && notifications[1]==before_first_pump[1]);
  CHECK(ku_task_driver_pump(&driver,1u,2u,&next)==KU_TASK_DRIVER_OK);
  CHECK(driver.workers[1].polls==2u && driver.workers[0].polls==1u);
  CHECK(ku_task_driver_pump(&driver,0u,2u,&next)==KU_TASK_DRIVER_OK);
  CHECK(!driver.queued && driver.workers[0].polls==2u);
  KuTaskDriverTicketV1 old=tasks[0].ticket;
  for(size_t i=0;i<4u;i++) {
    KuTaskAdapterOutcomeV1 outcome={0};
    CHECK(ku_task_value_take(&tasks[i],NULL,&outcome)==KU_TASK_CONTROL_OK);
    CHECK(outcome.result_kind==1u && outcome.value.integer.ok && outcome.value.integer.value==(int64_t)i);
    ku_task_outcome_drop(&outcome);
    CHECK(ku_task_value_drop(&tasks[i],deadline)==KU_TASK_DRIVER_OK);
  }
  for(size_t n=0;n<16u && driver.resident;n++)
    for(size_t owner=0;owner<2u;owner++) {
      uint32_t result=ku_task_driver_pump(&driver,owner,4u,&next);
      CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
    }
  CHECK(!driver.resident && !driver.reserved_bytes);
  KuTaskValueV1 work={0};
  CHECK(ku_task_2_start_value(&driver,&work)==KU_TASK_DRIVER_OK);
  size_t work_owner=slots[work.ticket.slot].owner_worker-1u;
  uint64_t before_polls=driver.workers[work_owner].polls;
  size_t before_notifications=notifications[work_owner];
  for(size_t turn=0;turn<16u && ku_task_control_status(&work.owner.lease)==KU_TASK_CONTROL_PENDING;turn++) {
    uint32_t result=ku_task_driver_pump(&driver,work_owner,64u,&next);
    CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
  }
  CHECK(ku_task_control_status(&work.owner.lease)==KU_TASK_CONTROL_COMPLETED);
  uint64_t used_polls=driver.workers[work_owner].polls-before_polls;
  CHECK(used_polls>100u && notifications[work_owner]==before_notifications);
  KuTaskAdapterOutcomeV1 work_result={0};
  CHECK(ku_task_value_take(&work,NULL,&work_result)==KU_TASK_CONTROL_OK);
  CHECK(work_result.value.integer.ok && work_result.value.integer.value==5000);
  ku_task_outcome_drop(&work_result);
  CHECK(ku_task_value_drop(&work,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<16u && driver.resident;turn++)
    CHECK(ku_task_driver_pump(&driver,work_owner,4u,&next)==KU_TASK_DRIVER_OK);
  CHECK(!driver.resident && !driver.reserved_bytes);
  KuTaskValueV1 replacement={0}; int64_t value=9;
  size_t before_new_publication[2]={notifications[0],notifications[1]};
  CHECK(ku_task_0_start_value(&driver,&value,&replacement)==KU_TASK_DRIVER_OK);
  size_t replacement_owner=slots[replacement.ticket.slot].owner_worker-1u;
  /* Publication after the final pump lock observes pumping==0 and retains its
   * actual directed wake, even before the embedding loop reaches OS poll. */
  CHECK(notifications[replacement_owner]>before_new_publication[replacement_owner]);
  CHECK(notifications[1u-replacement_owner]==before_new_publication[1u-replacement_owner]);
  CHECK(ku_task_driver_wake(&old)==KU_TASK_DRIVER_STALE);
  CHECK(ku_task_value_drop(&replacement,deadline)==KU_TASK_DRIVER_OK);
  size_t before_close[2]={notifications[0],notifications[1]};
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(notifications[0]>before_close[0] && notifications[1]>before_close[1]);
  for(size_t n=0;n<16u && driver.workers_exited<2u;n++)
    for(size_t owner=0;owner<2u;owner++) if(driver.workers[owner].state!=KU_TASK_DRIVER_WORKER_EXITED) {
      uint32_t result=ku_task_driver_pump(&driver,owner,4u,&next);
      CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
    }
  CHECK(!driver.resident && !driver.reserved_bytes && !driver.queued && driver.workers_exited==2u);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  /* A partial HTTP thread start leaves never-bound logical owners. With no
   * admitted work, the coordinator may bind/pump only these missing owners
   * after close, then reclaim through the ordinary EXITED/join contract. */
  memset(&driver,0,sizeof(driver));memset(slots,0,sizeof(slots));memset(ring,0,sizeof(ring));
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,4u,ring,4u,fixed+4u*sizeof(KuTaskInstance_0),2u,deadline,NULL,owner_wake)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t owner=0;owner<2u;owner++) {
    CHECK(!driver.workers[owner].owner_bound);
    uint32_t result=ku_task_driver_pump(&driver,owner,1u,&next);
    CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
  }
  CHECK(driver.workers_exited==2u && !driver.resident);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  puts("external-task-owners-ok"); return 0;
}
"#
    );
    let directory = TempDir::new("task-external-owner");
    let path = directory.path().join("external.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "external") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "external-task-owners-ok\n"
    );
}

const MOVES_AND_AWAITS: &str = r#"
async fn main(): null! {
    first = Child(3)
    second = Child(4)
    moved = first
    a = (await moved)?
    b = (await second)?
    text = (await Text("Ku 世界"))?
    println(a)
    println(b)
    println(text)
    return ok(null)
}
async fn Child(value: int): int! { return ok(value) }
async fn Text(value: str): str! { return ok(value) }
"#;

#[test]
fn native_task_source_generated_start_await_and_scope_execute_without_source() {
    let cases = [
        (MOVES_AND_AWAITS, "3\n4\nKu 世界\n", "", true),
        (
            "async fn main(): null! { first = Child(1) second = Child(2) third = Child(3) return ok(null) } async fn Child(value: int): int! { return ok(value) }",
            "", "", true,
        ),
        (
            "async fn Flag(value: bool): bool! { return ok(value) } async fn Empty(): null! { return ok(null) } async fn main(): null! { flag = (await Flag(true))? empty = (await Empty())? print(flag) println(empty) return ok(null) }",
            "truenull\n", "", true,
        ),
        (
            "async fn Broken(): int! { fail \"expected failure\" } async fn main(): null! { child = Broken() result = await child println(7) return ok(null) }",
            "7\n", "", true,
        ),
        (
            "async fn Broken(): int! { fail \"expected failure\" } async fn main(): null! { child = Broken() value = (await child)? println(value) return ok(null) }",
            "", "expected failure\n", false,
        ),
        (
            "async fn Number(): int! { return ok(8) } async fn Carry(text: str, value: int): str! { println(value) return ok(text) } async fn main(): null! { text = (await Carry(\"argument before await\", (await Number())?))? println(text) return ok(null) }",
            "8\nargument before await\n", "", true,
        ),
        (
            "async fn Unwrap(value: str!): str! { return value } async fn main(): null! { wrapped = ok(\"a\0b 世界\") text = (await Unwrap(wrapped))? println(text) return ok(null) }",
            "a\0b 世界\n", "", true,
        ),
        (
            "async fn println(value: int): null! { return ok(null) } async fn main(): null! { ignored = println(7) return ok(null) }",
            "", "", true,
        ),
        (
            "async fn main(): null! { text = \"kept\" text println(text) return ok(null) }",
            "kept\n", "", true,
        ),
        (
            "async fn ok(value: int): null! { fail \"user ok failed\" } async fn main(): null! { child = ok(7) result = await child fail \"main done\" }",
            "", "main done\n", false,
        ),
    ];
    for (index, (source, expected_out, expected_err, success)) in cases.into_iter().enumerate() {
        let generated = native(source);
        for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
            assert!(!generated.contains(forbidden));
        }
        assert!(generated.contains("ku_task_driver_wait_result("));
        assert!(generated.contains("ku_task_root_driver"));
        let directory = TempDir::new(&format!("native-task-source-{index}"));
        let path = directory.path().join("program.c");
        fs::write(&path, generated).unwrap();
        let Some(executable) = compile_harness(directory.path(), &path, "program") else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must execute native source Tasks"
            );
            continue;
        };
        fs::remove_file(path).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .expect("native source Tasks terminate within the bounded process watchdog");
        assert_eq!(
            output.status.success(),
            success,
            "case {index}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            expected_out,
            "case {index}"
        );
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().replace('\r', ""),
            expected_err,
            "case {index}"
        );
    }
}

#[test]
fn native_task_source_lowering_keeps_pending_away_from_argument_and_move_operations() {
    use ku::ir::task::{TaskOp, TaskTerminator};
    let native = task_lower::lower_program(&checked(MOVES_AND_AWAITS)).unwrap();
    let main = native
        .tasks
        .functions
        .iter()
        .find(|function| function.id == native.entry)
        .unwrap();
    let awaits = main
        .states
        .iter()
        .filter(|state| matches!(state.terminator, TaskTerminator::Await { .. }))
        .collect::<Vec<_>>();
    assert_eq!(awaits.len(), 3);
    assert!(awaits.iter().all(|state| state.operations.is_empty()));
    assert_eq!(
        main.states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|operation| matches!(operation, TaskOp::Start { .. }))
            .count(),
        3
    );
}

#[test]
fn native_task_source_exit_owns_normal_results_without_erasing_await_cleanup() {
    use ku::ir::{
        task::{self, SlotId, TaskConstant, TaskLimits, TaskOp, TaskSlotType, TaskTerminator},
        IrType,
    };

    let source = r#"
async fn ReturnText(value: str): str! { local = value return ok(local) }
async fn FailText(): str! { retained = "held" fail "expected failure" }
async fn Question(value: str!): str! { local = value? return ok(local) }
async fn ShortAnd(gate: bool): bool! { return ok(gate && (await Flag())?) }
async fn ShortOr(gate: bool): bool! { return ok(gate || (await Flag())?) }
async fn Flag(): bool! { return ok(true) }
async fn AwaitText(value: str): str! {
    retained = "held across await"
    child = ReturnText(value)
    output = (await child)?
    println(retained)
    return ok(output)
}
async fn main(): null! { return ok(null) }
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let mut await_count = 0;
    for function in &native.tasks.functions {
        let frame = plan
            .functions
            .iter()
            .find(|frame| frame.function == function.id)
            .unwrap();
        assert!(frame.exit_bridge && frame.hosted, "{}", function.name);
        let owned: Vec<_> = function
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                matches!(&slot.ty, TaskSlotType::Value { ty, borrowed: false }
                    if matches!(ty, IrType::Str | IrType::Result(_)))
            })
            .map(|(index, _)| SlotId(index))
            .collect();
        assert!(owned.iter().all(|slot| frame.slots.contains(slot)));
        let cleanup: Vec<_> = owned
            .iter()
            .rev()
            .map(|slot| TaskOp::DropIfInit { slot: *slot })
            .collect();
        let mut exit_count = 0;
        for state in &function.states {
            match state.terminator {
                TaskTerminator::Exit { value } => {
                    exit_count += 1;
                    assert_eq!(
                        function.slots[value.0].ty,
                        TaskSlotType::Value {
                            ty: function.result.clone(),
                            borrowed: false,
                        }
                    );
                    assert!(
                        state
                            .operations
                            .iter()
                            .all(|operation| { !matches!(operation, TaskOp::DropIfInit { .. }) }),
                        "{} normal Exit must leave remaining Values for handoff-first glue",
                        function.name
                    );
                }
                TaskTerminator::Complete { .. } => {
                    panic!("{} still uses legacy normal completion", function.name)
                }
                TaskTerminator::Await {
                    cleanup: cleanup_state,
                    ..
                } => {
                    await_count += 1;
                    assert!(state.operations.is_empty(), "Pending must not replay setup");
                    let block = &function.states[cleanup_state.0];
                    assert_eq!(block.terminator, TaskTerminator::Terminate);
                    // Keep every conditional Value drop in reverse declaration
                    // order, including slots only initialized on another edge.
                    // Task owners are deliberately absent: the host hands them
                    // off before entering this cancellation-only successor.
                    assert_eq!(block.operations, cleanup, "{}", function.name);
                }
                _ => {}
            }
        }
        assert!(exit_count > 0, "{} has no normal Exit", function.name);
    }
    assert_eq!(await_count, 3);

    let failed = native
        .tasks
        .functions
        .iter()
        .find(|function| function.name == "FailText")
        .unwrap();
    let state = &failed.states[failed.entry.0];
    let TaskTerminator::Exit { value } = state.terminator else {
        panic!("fail must stage a normal user Result");
    };
    assert!(state.operations.iter().any(|operation| {
        matches!(operation, TaskOp::Init {
            dst,
            value: TaskConstant::Err { domain, code, message, .. },
        } if *dst == value && domain == "ku" && code == "fail" && message == "expected failure")
    }));

    let question = native
        .tasks
        .functions
        .iter()
        .find(|function| function.name == "Question")
        .unwrap();
    let TaskTerminator::TryResult {
        ok_value,
        err_result,
        ok,
        err,
        ..
    } = question.states[question.entry.0].terminator
    else {
        panic!("? must retain both typed Result successors");
    };
    assert_eq!(
        question.states[err.0].terminator,
        TaskTerminator::Exit { value: err_result }
    );
    let success = &question.states[ok.0];
    assert!(matches!(success.terminator, TaskTerminator::Exit { .. }));
    assert!(success
        .operations
        .iter()
        .any(|operation| matches!(operation, TaskOp::Move { src, .. } if *src == ok_value)));

    for (name, right_on_true) in [("ShortAnd", true), ("ShortOr", false)] {
        let function = native
            .tasks
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        let TaskTerminator::Branch {
            then_state,
            else_state,
            ..
        } = function.states[function.entry.0].terminator
        else {
            panic!("{name} lost short-circuit control flow");
        };
        let (right, join) = if right_on_true {
            (then_state, else_state)
        } else {
            (else_state, then_state)
        };
        let skipped = &function.states[join.0];
        assert!(matches!(skipped.terminator, TaskTerminator::Exit { .. }));
        assert!(skipped
            .operations
            .iter()
            .all(|operation| !matches!(operation, TaskOp::Start { .. })));
        assert_eq!(
            function.states[right.0]
                .operations
                .iter()
                .filter(|operation| matches!(operation, TaskOp::Start { .. }))
                .count(),
            1
        );
        let TaskTerminator::Jump { target: poll } = function.states[right.0].terminator else {
            panic!("{name} RHS must set up its child before a separate Await");
        };
        let TaskTerminator::Await { ready, .. } = function.states[poll.0].terminator else {
            panic!("{name} RHS lost Await");
        };
        let TaskTerminator::TryResult {
            ok,
            err,
            err_result,
            ..
        } = function.states[ready.0].terminator
        else {
            panic!("{name} RHS must propagate the awaited Result");
        };
        assert_eq!(
            function.states[err.0].terminator,
            TaskTerminator::Exit { value: err_result }
        );
        assert_eq!(
            function.states[ok.0].terminator,
            TaskTerminator::Jump { target: join }
        );
        assert!(function.states[ok.0]
            .operations
            .iter()
            .any(|operation| matches!(operation, TaskOp::Copy { .. })));
    }
}

#[test]
fn native_task_source_zero_task_exit_persists_owned_locals_not_dead_copy_temporaries() {
    use ku::ir::{
        task::{self, SlotId, TaskLimits, TaskOp, TaskSlotType, TaskTerminator},
        IrType,
    };

    // A legitimate typed host may supply Owned storage for value. This source
    // only moves it to a local; it does not introduce source heap-allocation syntax.
    let source = r#"
async fn Local(value: str): str! {
    local = value
    number = 7
    println(number)
    return ok(local)
}
async fn main(): null! { return ok(null) }
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let function = &native.tasks.functions[0];
    assert_eq!(function.name, "Local");
    let frame = plan
        .functions
        .iter()
        .find(|frame| frame.function == function.id)
        .unwrap();
    assert!(frame.exit_bridge && frame.hosted);
    assert_eq!(frame.scope_task_mask, 0);
    assert!(frame.suspensions.is_empty());
    let state = &function.states[function.entry.0];
    assert!(matches!(state.terminator, TaskTerminator::Exit { .. }));
    let moved_local = state
        .operations
        .iter()
        .find_map(|operation| match operation {
            TaskOp::Move { dst, src } if function.parameters.contains(src) => Some(*dst),
            _ => None,
        })
        .expect("source binding must move the owned parameter to a distinct local");
    assert!(!function.parameters.contains(&moved_local));
    assert!(frame.slots.contains(&moved_local));
    let mut dead_copy = 0;
    let mut dead_null = 0;
    for (index, slot) in function.slots.iter().enumerate() {
        let id = SlotId(index);
        match &slot.ty {
            TaskSlotType::Value {
                ty: IrType::Int,
                borrowed: false,
            } if !function.parameters.contains(&id) => {
                dead_copy += 1;
                assert!(
                    !frame.slots.contains(&id),
                    "dead Copy slot {index} was spilled"
                );
            }
            TaskSlotType::Value {
                ty: IrType::Null,
                borrowed: false,
            } => {
                dead_null += 1;
                assert!(
                    !frame.slots.contains(&id),
                    "unused print result {index} was spilled"
                );
                assert!(state.operations.iter().any(|operation| {
                    matches!(operation, TaskOp::Init { dst, value: task::TaskConstant::Null } if *dst == id)
                }));
            }
            TaskSlotType::Value {
                ty: IrType::Str | IrType::Result(_),
                borrowed: false,
            } => assert!(
                frame.slots.contains(&id),
                "Owned slot {index} was not persisted"
            ),
            other => panic!("unexpected zero-Task fixture slot: {other:?}"),
        }
    }
    assert_eq!(
        dead_copy, 2,
        "literal and local Copy snapshots must both exist"
    );
    assert_eq!(dead_null, 1, "println has one unused null result");
}

#[test]
fn native_task_source_subset_rejects_unimplemented_constructs() {
    let sources = [
        "async fn main(): null! { for item in [[1], [2]] {} return ok(null) }",
        "async fn main(): null! { try { println(1) } finally { for item in 0 { println(item) } } return ok(null) }",
        "fn Sync(): int { return 1 } async fn main(): null! { return ok(null) }",
        "async fn Child(): int! { return ok(1) } async fn main(): null! { chosen = Child chosen = Child return ok(null) }",
        "async fn Child(): int! { return ok(1) } async fn main(): null! { Child() return ok(null) }",
        "async fn main(): int! { return ok(1) }",
        "async fn Child(): int! { value = (await Child())? return ok(value) } async fn main(): null! { return ok(null) }",
    ];
    for source in sources {
        let error = task_lower::lower_program(&checked(source))
            .expect_err("unimplemented source must be rejected");
        assert!(
            error.message.contains("not support") || error.message.contains("recursive"),
            "{error}"
        );
    }
}

#[test]
fn native_task_source_ordinary_synchronous_artifacts_remain_runtime_free() {
    let ast = checked("fn main() { println(7) }");
    assert!(!task_lower::has_async_entry(&ast));
    let generated = c::generate_c_source(&ku::ir::lower_program(&ast).unwrap()).unwrap();
    for forbidden in [
        "KuTaskValue",
        "ku_task_driver_",
        "ku_task_root_",
        "_beginthreadex",
        "pthread_create",
    ] {
        assert!(
            !generated.contains(forbidden),
            "synchronous source gained Task runtime: {forbidden}"
        );
    }
}

#[test]
fn native_task_source_cli_imports_build_and_run_after_source_graph_moves() {
    for emit_only in [true, false] {
        let directory = TempDir::new("native-task-cli-import");
        let sources = directory.path().join("sources");
        fs::create_dir(&sources).unwrap();
        fs::write(
            sources.join("worker.ku"),
            "import time from \"std.time\"\nasync fn Text(value: str): str! { time.sleep(1)? return ok(value) }",
        )
        .unwrap();
        let main = sources.join("main.ku");
        fs::write(&main, "import { Text } from \"./worker.ku\"\nasync fn main(): null! { for item in 3 { if (item == 0) { continue } result = (await Text(\"import 世界\"))? println(result) break } return ok(null) }").unwrap();
        let binary = directory
            .path()
            .join(if cfg!(windows) { "app.exe" } else { "app" });
        let mut command = Command::new(env!("CARGO_BIN_EXE_ku"));
        command.current_dir(directory.path()).arg("build");
        if emit_only {
            command.arg("--native").arg(&main);
        } else {
            command
                .args(["--backend", "c"])
                .arg(&main)
                .arg("-o")
                .arg(&binary);
        }
        let built = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_LIMITS).unwrap();
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr)
        );
        assert!(
            built.status.success() || (!emit_only && diagnostics.contains("C compiler not found")),
            "{diagnostics}"
        );
        let artifact = if emit_only {
            main.with_extension("c")
        } else {
            let root = sources.join(".ku/build/debug/c");
            let artifacts: Vec<_> = fs::read_dir(&root)
                .unwrap()
                .take(257)
                .map(|entry| entry.unwrap().path().join("app.c"))
                .filter(|path| path.is_file())
                .collect();
            assert_eq!(
                artifacts.len(),
                1,
                "expected one output-digest C artifact under {}",
                root.display()
            );
            artifacts[0].clone()
        };
        let generated = fs::read_to_string(&artifact)
            .unwrap_or_else(|error| panic!("{}: {error}\n{diagnostics}", artifact.display()));
        assert!(
            generated.contains("ku_task_root_driver") && generated.contains("ku_task_host_await")
        );
        assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
        let detached = directory.path().join("detached.c");
        fs::write(&detached, generated).unwrap();
        fs::rename(&sources, directory.path().join("moved-sources")).unwrap();
        assert!(!sources.exists());
        let executable = if emit_only {
            compile_harness(directory.path(), &detached, "app")
        } else if built.status.success() {
            Some(binary)
        } else {
            None
        };
        let Some(executable) = executable else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must link and run native Task CLI artifacts"
            );
            continue;
        };
        fs::remove_file(detached).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            "import 世界\n"
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn native_task_source_cli_rejects_unsupported_and_moved_values_before_artifacts() {
    let sources = [
        "async fn main(): null! { try { println(1) } finally { while (false) { println(0) } } return ok(null) }",
        "async fn Child(): str! { return ok(\"child\") } async fn main(): null! { child = Child() moved = child value = await child return ok(null) }",
        "async fn Child(value: str): str! { return ok(value) } async fn main(): null! { value = \"owned\" child = Child(value) println(value) return ok(null) }",
    ];
    for source in sources {
        let directory = TempDir::new("native-task-cli-reject");
        let main = directory.path().join("main.ku");
        fs::write(&main, source).unwrap();
        let error = ku::cli::run_cli(vec![
            "ku".into(),
            "build".into(),
            "--native".into(),
            main.to_string_lossy().into_owned(),
        ])
        .expect_err("unsupported or invalid ownership must not produce native code");
        assert!(
            error.message.contains("not support")
                || error.message.contains("moved")
                || error.message.contains("has already been awaited"),
            "{error}"
        );
        assert!(!main.with_extension("c").exists());
    }
}

#[test]
fn native_task_source_output_failure_unwinds_await_without_question_mark() {
    let generated = native("async fn Write(): int! { print(3) return ok(1) } async fn main(): null! { result = await Write() println(7) return ok(null) }");
    let anchor = "typedef struct KuString {";
    assert_eq!(generated.matches(anchor).count(), 1);
    // A real generated Print calls the standard flush and then receives a
    // deterministic I/O failure. No replacement Start/Await/cleanup callback.
    let source = generated.replacen(
        anchor,
        &format!(
            r#"
static int ku_fixture_flush(FILE* stream) {{
  int status=fflush(stream);
  return stream==stdout ? EOF : status;
}}
#define fflush ku_fixture_flush
{anchor}"#
        ),
        1,
    );
    let directory = TempDir::new("native-task-output-failure");
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run native Task output failure"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"3");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().replace('\r', ""),
        "output write failed\n"
    );
}

#[test]
fn native_task_source_diagnostic_write_failure_still_destroys_runtime() {
    let generated = native("async fn Child(): int! { return ok(1) } async fn main(): null! { child = Child() fail \"expected failure\" }");
    let anchor = "typedef struct KuString {";
    assert_eq!(generated.matches(anchor).count(), 1);
    let source = generated.replacen(
        anchor,
        &format!(
            r#"
static size_t ku_fixture_write(const void* data, size_t size, size_t count, FILE* stream) {{
  return stream==stderr ? 0 : fwrite(data,size,count,stream);
}}
#define fwrite ku_fixture_write
{anchor}"#
        ),
        1,
    );
    let done = "  return exit_code;\n}\n";
    assert_eq!(source.matches(done).count(), 1);
    let source = source.replacen(
        done,
        "  fputs(\"runtime destroyed\\n\",stdout);\n  return exit_code;\n}\n",
        1,
    );
    let directory = TempDir::new("native-task-diagnostic-failure");
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run native Task diagnostic failure"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "runtime destroyed\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().replace('\r', ""),
        "\n"
    );
}

#[test]
fn native_task_sleep_recognizes_only_imported_unshadowed_std_time_integer_calls() {
    use ku::ir::task::{SlotId, TaskTerminator};
    for import in [
        "import time from \"std.time\"",
        "import \"std.time\"",
        "import { time } from \"std\"",
    ] {
        // Exercise the lowerer's import boundary directly as well as the CLI's
        // separately covered synthetic std:time capability after expansion.
        let source = format!(
            "{import}\nasync fn main(): null! {{ ms = 1 time.sleep(ms)? return ok(null) }}"
        );
        let ast = Parser::new(Lexer::new(&source).lex().unwrap())
            .parse_program()
            .unwrap();
        let lowered = task_lower::lower_program(&ast).expect("supported standard time import");
        let function = &lowered.tasks.functions[0];
        let waits: Vec<_> = function
            .states
            .iter()
            .filter_map(|state| match state.terminator {
                TaskTerminator::Timer { millis, dst, .. } => Some((millis, dst)),
                _ => None,
            })
            .collect();
        assert_eq!(waits.len(), 1);
        assert_ne!(
            waits[0].0,
            SlotId(0),
            "milliseconds use an immutable materialized snapshot"
        );
        assert_ne!(waits[0].0, waits[0].1);
    }
    for source in [
        "async fn main(): null! { time.sleep(0)? return ok(null) }",
        "import time from \"./time.ku\"\nasync fn main(): null! { time.sleep(0)? return ok(null) }",
        "import clock from \"std.time\"\nasync fn main(): null! { clock.sleep(0)? return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time = 1 time.sleep(0)? return ok(null) }",
        "import time from \"std.time\"\nasync fn Child(time: int): null! { time.sleep(0)? return ok(null) } async fn main(): null! { return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time.now() return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time.sleep(true)? return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time.sleep({ milliseconds: 1 })? return ok(null) }",
    ] {
        let ast=Parser::new(Lexer::new(source).lex().unwrap()).parse_program().unwrap();
        assert!(task_lower::lower_program(&ast).is_err(),"must preserve rejected module/type boundary: {source}");
    }
}

#[test]
fn native_task_read_dir_owns_large_sorted_outputs_and_preserves_paths() {
    let source = r#"
import fs from "std.fs"
async fn Copy(values: [str]): [str]! { return ok(values) }
async fn List(path: str): [str]! {
    first = fs.read_dir(path)?
    again = fs.read_dir(path)?
    println(again.len())
    return ok(first)
}
async fn main(): null! {
    listed = (await List("listing"))?
    cloned = listed.clone()
    moved = (await Copy(cloned))?
    println(moved.len())
    println(listed.try_get(0)?)
    println(moved.try_get(81)?)
    empty = fs.read_dir("empty")?
    println(empty.len())
    try { fs.read_dir("missing")? } catch(err) { println(err.code) }
    return ok(null)
}
"#;
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:fs".into(),
            span: Default::default(),
        }),
    );
    Checker::new().check(&ast).unwrap();
    let lowered = task_lower::lower_program(&ast).unwrap();
    let generated = c::generate_native_task_c_source(&lowered, &Default::default()).unwrap();
    assert!(generated.contains("ku_task_host_blocking_read_dir("));
    let directory = TempDir::new("native-task-directory");
    fs::create_dir(directory.path().join("listing")).unwrap();
    fs::create_dir(directory.path().join("empty")).unwrap();
    for index in 0..80 {
        fs::write(directory.path().join(format!("listing/f{index:03}")), b"").unwrap();
    }
    fs::write(directory.path().join("listing/中文😀"), b"").unwrap();
    fs::write(directory.path().join("listing/é"), b"").unwrap();
    let path = directory.path().join("directory.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "directory") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires directory Task execution"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout)
        .unwrap()
        .replace('\r', "")
        .replace('\\', "/");
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 6, "{text}");
    assert_eq!(lines[0], "82");
    assert_eq!(lines[1], "82");
    assert!(lines[2].ends_with("listing/f000"), "{text}");
    assert!(lines[3].ends_with("listing/中文😀"), "{text}");
    assert_eq!(lines[4], "0");
    assert_eq!(lines[5], "read_dir_failed");
}

fn native_task_transport_source(source: &str) -> String {
    // Ku accepts embedded NUL bytes; its string escape set has no \0 spelling.
    let source = source.replace("\\0", "\0");
    let mut ast = Parser::new(Lexer::new(&source).lex().expect("transport source lexes"))
        .parse_program()
        .expect("transport source parses");
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    for name in ["std:net", "std:bytes", "std:time"] {
        ast.items.insert(
            0,
            ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: name.into(),
                span: Default::default(),
            }),
        );
    }
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    let tasks = task_lower::lower_program(&ast).expect("transport source lowers");
    c::generate_native_task_c_source(&tasks, &Default::default()).expect("transport C generation")
}

#[test]
fn native_task_tcp_wait_yields_one_executor_and_preserves_partial_binary_data() {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let source = format!(
        r#"
import net from "std.net"
import bytes from "std.bytes"
import time from "std.time"
async fn Tick(): null! {{ time.sleep(10)? println("tick") return ok(null) }}
async fn Exchange(port: int): null! {{
    client=net.client({{host:"127.0.0.1",port:port,tls:false,read_timeout_ms:2000}})?
    sent=bytes.from_str("A\0B")?
    client.write(sent)?
    println(sent.len())
    total=0
    sum=0
    while (total<3) {{
        part=client.read(3)?
        for index in part.len() {{ sum=sum+part.get(index)? }}
        total=total+part.len()
    }}
    println(total)
    println(sum)
    try {{ client.read(1)? }} catch(err) {{ println(err.code) }}
    client.close()
    return ok(null)
}}
async fn main(): null! {{
    exchange=Exchange({port})
    timer=Tick()
    timer_done=(await timer)?
    exchange_done=(await exchange)?
    return ok(null)
}}
"#
    );
    let mut generated = native_task_transport_source(&source);
    assert!(!generated.contains("#define KU_FEATURE_NATIVE_TLS 1"));
    assert!(generated.contains("#define KU_TASK_BLOCKING_NETWORK 1"));
    assert!(generated.contains("#define KU_TASK_BLOCKING_FILES 0"));
    // One executor makes a blocking call on that executor observable: Tick
    // must run before the delayed peer delivers the remaining response bytes.
    let start = generated
        .find("static size_t ku_task_root_worker_count(void) {")
        .unwrap();
    let end = start
        + generated[start..]
            .find("static uint64_t ku_task_root_deadline(")
            .unwrap();
    generated.replace_range(
        start..end,
        "static size_t ku_task_root_worker_count(void) { return 1u; }\n",
    );
    let directory = TempDir::new("native-task-tcp-yield");
    let path = directory.path().join("transport.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "transport") else {
        return;
    };
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("bounded Task transport accept: {error}"),
            }
        };
        // Accepted sockets may inherit the listener's nonblocking mode. This peer
        // uses bounded blocking I/O; a read timeout does not change that mode.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut input = [0u8; 3];
        stream.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"A\0B");
        std::thread::sleep(Duration::from_millis(100));
        stream.write_all(b"A").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        stream.write_all(b"\0B").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
    });
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    peer.join().expect("Task transport peer exits");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(
        &lines[lines.len() - 3..],
        &["3", "131", "end_of_stream"],
        "{stdout:?}"
    );
    assert!(lines[..lines.len() - 3].contains(&"tick"));
    assert!(lines[..lines.len() - 3].contains(&"3"));
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_local_bytes_use_existing_errors_and_owned_results() {
    let source = r#"
import net from "std.net"
import bytes from "std.bytes"
async fn Numbers(): null! {
    input=bytes.from_array([65,0,255])?
    cloned=input.clone()
    println(input.len())
    println(cloned.get(1)?)
    println(cloned.get(2)?)
    try { cloned.to_str()? } catch(err) { println(err.code) }
    try { cloned.get(3)? } catch(err) { println(err.code) }
    return ok(null)
}
async fn More(): null! {
    try { bytes.from_array([256])? } catch(err) { println(err.code) }
    text=bytes.from_str("中文😀\0尾")?
    println(text.to_str()?)
    try { net.client({host:"127.0.0.1",port:0})? } catch(err) { println(err.code) }
    return ok(null)
}
async fn main(): null! {
    numbers_done=(await Numbers())?
    more_done=(await More())?
    return ok(null)
}
"#;
    let generated = native_task_transport_source(source);
    let directory = TempDir::new("native-task-local-bytes");
    let path = directory.path().join("bytes.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "bytes") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "3\n0\n255\ninvalid_utf8\nindex_out_of_bounds\ninvalid_byte\n中文😀\0尾\ninvalid_config\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_tcp_late_readiness_cannot_mutate_a_reused_job_generation() {
    let generated = native_task_transport_source(
        r#"
import net from "std.net"
async fn main(): null! {
    client=net.client({host:"127.0.0.1",port:9,tls:false})?
    client.close()
    return ok(null)
}
"#,
    )
    .replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    );
    let source = format!(
        "{generated}\n{}",
        r#"
#define CHECK(c) do {if(!(c)){fprintf(stderr,"late TCP readiness line %d: %s\n",__LINE__,#c);return 1;}}while(0)
int main(void) {
  /* The fixed 1024-job table alone exceeds a Windows default 1MiB main
     stack. Production roots use static/heap storage; mirror that here. */
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)calloc(1,sizeof(*pool));
  CHECK(pool);uint64_t deadline=ku_task_driver_now_ms()+1000u;
  CHECK(ku_task_blocking_init(pool,1u,1u,deadline)==KU_TASK_DRIVER_OK);
  CHECK(!ku_task_blocking_lock(pool));
  KuTaskBlockingJobV1* job=&pool->jobs[0];job->generation=2u;job->state=KU_TASK_BLOCKING_NET_WAIT;
  job->net_owner=1u;job->kind=6u;job->net_requested=17;job->net_deadline=deadline;
  ku_task_blocking_unlock(pool);
  CHECK(ku_task_net_dispatch(pool,0u,0u,1u,KU_TASK_NET_READ|KU_TASK_NET_ERROR)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_net_dispatch(pool,1u,0u,2u,KU_TASK_NET_READ)==KU_TASK_DRIVER_OK);
  CHECK(!ku_task_blocking_lock(pool));
  CHECK(job->generation==2u && job->state==KU_TASK_BLOCKING_NET_WAIT && job->net_requested==17 && !job->cancelled && !pool->fault);
  memset(job,0,sizeof(*job));ku_task_blocking_unlock(pool);
  CHECK(ku_task_blocking_finish(pool,deadline)==KU_TASK_DRIVER_OK);
  CHECK(pool->initialized==KU_TASK_DRIVER_STORAGE_ZERO && !pool->resources
      && !pool->resident && !pool->queued && !pool->running
      && pool->workers_joined==pool->workers_created);
  free(pool);
  puts("late-tcp-generation-ok");return 0;
}
"#
    );
    let directory = TempDir::new("tcp-late-generation");
    let path = directory.path().join("generation.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "generation") else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "late TCP readiness process failed: {} ({:?}, code {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.status,
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: &[u8] = if cfg!(windows) {
        b"late-tcp-generation-ok\r\n"
    } else {
        b"late-tcp-generation-ok\n"
    };
    assert!(output.stdout.ends_with(expected));
}

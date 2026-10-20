//! The actual emitted checked-i64 helpers, not a reimplementation in the C test.
//! Expected results come from Rust checked operations. This does not replace
//! Task source/runtime-failure, cancellation or final-drain integration tests.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, task::*, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fmt::Write as _, fs, process::Command};

const OK: u32 = 0;
const OVERFLOW: u32 = 1;
const DIV_ZERO: u32 = 2;
const HELPERS: [&str; 6] = [
    "ku_int_neg",
    "ku_int_add",
    "ku_int_sub",
    "ku_int_mul",
    "ku_int_div",
    "ku_int_rem",
];

fn sync_program() -> ir::IrProgram {
    let ast = Parser::new(Lexer::new("fn main() {}").tokenize().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    ir::lower_program(&ast).unwrap()
}

fn value_slot(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}

fn arithmetic_frames() -> TaskProgram {
    let result = IrType::Result(Box::new(IrType::Int));
    let binary = [
        TaskBinaryOp::Add,
        TaskBinaryOp::Subtract,
        TaskBinaryOp::Multiply,
        TaskBinaryOp::Divide,
        TaskBinaryOp::Remainder,
    ];
    let mut functions: Vec<_> = binary
        .into_iter()
        .enumerate()
        .map(|(id, op)| TaskFunction {
            id: TaskFunctionId(id),
            name: format!("CheckedInteger{id}"),
            slots: vec![
                value_slot(IrType::Int),
                value_slot(IrType::Int),
                value_slot(IrType::Int),
                value_slot(result.clone()),
            ],
            parameters: vec![SlotId(0), SlotId(1)],
            entry: StateId(0),
            result: result.clone(),
            states: vec![TaskState {
                operations: vec![
                    TaskOp::Binary {
                        dst: SlotId(2),
                        op,
                        left: SlotId(0),
                        right: SlotId(1),
                    },
                    TaskOp::WrapOk {
                        dst: SlotId(3),
                        src: SlotId(2),
                    },
                ],
                terminator: TaskTerminator::Complete { value: SlotId(3) },
            }],
        })
        .collect();
    functions.push(TaskFunction {
        id: TaskFunctionId(5),
        name: "CheckedNegation".into(),
        slots: vec![
            value_slot(IrType::Int),
            value_slot(IrType::Int),
            value_slot(result.clone()),
        ],
        parameters: vec![SlotId(0)],
        entry: StateId(0),
        result,
        states: vec![TaskState {
            operations: vec![
                TaskOp::Unary {
                    dst: SlotId(1),
                    op: TaskUnaryOp::Negate,
                    src: SlotId(0),
                },
                TaskOp::WrapOk {
                    dst: SlotId(2),
                    src: SlotId(1),
                },
            ],
            terminator: TaskTerminator::Complete { value: SlotId(2) },
        }],
    });
    TaskProgram { functions }
}

fn comparison_frames() -> TaskProgram {
    let result = IrType::Result(Box::new(IrType::Bool));
    TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "ComparisonAndNot".into(),
            slots: vec![
                value_slot(IrType::Int),
                value_slot(IrType::Int),
                value_slot(IrType::Bool),
                value_slot(IrType::Bool),
                value_slot(result.clone()),
            ],
            parameters: vec![SlotId(0), SlotId(1)],
            entry: StateId(0),
            result,
            states: vec![TaskState {
                operations: vec![
                    TaskOp::Binary {
                        dst: SlotId(2),
                        op: TaskBinaryOp::Less,
                        left: SlotId(0),
                        right: SlotId(1),
                    },
                    TaskOp::Unary {
                        dst: SlotId(3),
                        op: TaskUnaryOp::Not,
                        src: SlotId(2),
                    },
                    TaskOp::WrapOk {
                        dst: SlotId(4),
                        src: SlotId(3),
                    },
                ],
                terminator: TaskTerminator::Complete { value: SlotId(4) },
            }],
        }],
    }
}

fn generated_helpers() -> String {
    let tasks = arithmetic_frames();
    let plan = verify_and_plan(&tasks, TaskLimits::default()).unwrap();
    assert!(plan.functions.iter().all(|function| !function.hosted));
    let source = c::generate_task_frame_c_source(&sync_program(), &tasks).unwrap();
    for helper in HELPERS {
        assert_eq!(
            source
                .matches(&format!("static uint32_t {helper}("))
                .count(),
            1,
            "{helper} must be emitted once for all six functions"
        );
    }
    let start = source
        .find("/* Private checked int64_t computations.")
        .expect("the actual shared integer runtime was emitted");
    let end = source[start..]
        .find("typedef char KuSyncStatusContract[")
        .expect("shared emitter checks the status contract immediately after integer helpers")
        + start;
    let helpers = &source[start..end];
    // Task and synchronous functions share this one early helper block. Do not
    // include intervening user/runtime functions up to the later Task ABI.
    for helper in HELPERS {
        assert_eq!(
            helpers
                .matches(&format!("static uint32_t {helper}("))
                .count(),
            1
        );
    }
    for forbidden in [
        "malloc(",
        "calloc(",
        "realloc(",
        "free(",
        "exit(",
        "abort(",
        "__int128",
        "__builtin_",
    ] {
        assert!(!helpers.contains(forbidden), "helper used {forbidden}");
    }
    for forbidden in ["run_source", "const SOURCE"] {
        assert!(!source.contains(forbidden));
    }
    source
}

#[test]
fn native_task_integer_helpers_are_emitted_once_and_only_for_arithmetic() {
    generated_helpers();
    let sync = sync_program();
    for source in [
        c::generate_c_source(&sync).unwrap(),
        c::generate_task_frame_c_source(&sync, &comparison_frames()).unwrap(),
    ] {
        assert!(!source.contains("ku_int_"));
        assert!(!source.contains("KU_INT_"));
        assert!(!source.contains("Private checked int64_t computations"));
    }
}

fn boundary_values() -> Vec<i64> {
    let values = vec![
        i64::MIN,
        i64::MIN + 1,
        i64::MIN / 2 - 1,
        i64::MIN / 2,
        i64::MIN / 2 + 1,
        -3_037_000_500,
        -3_037_000_499,
        -3,
        -2,
        -1,
        0,
        1,
        2,
        3,
        3_037_000_499,
        3_037_000_500,
        i64::MAX / 2,
        i64::MAX - 1,
        i64::MAX,
        // Truncation-sensitive multiplication limits on both sides of zero.
        i64::MAX / 3,
        i64::MAX / 3 + 1,
        i64::MIN / 3,
        i64::MIN / 3 - 1,
    ];
    assert_eq!(values.len(), 23);
    assert_eq!(
        values
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        23
    );
    values
}

fn expected_binary(operation: usize, left: i64, right: i64) -> (u32, i64) {
    // checked_div/checked_rem return None for two DIFFERENT reasons. The Ku
    // zero-divisor decision takes precedence, including MIN with a zero RHS.
    if matches!(operation, 3 | 4) && right == 0 {
        return (DIV_ZERO, 0);
    }
    let result = match operation {
        0 => left.checked_add(right),
        1 => left.checked_sub(right),
        2 => left.checked_mul(right),
        3 => left.checked_div(right),
        4 => left.checked_rem(right),
        _ => panic!("unknown test arithmetic operation"),
    };
    result.map_or((OVERFLOW, 0), |value| (OK, value))
}

fn c_integer(value: i64) -> String {
    // Never emit the lexer/C token whose positive magnitude exceeds MAX.
    if value == i64::MIN {
        "INT64_MIN".into()
    } else if value < 0 {
        format!("(-INT64_C({}))", value.checked_neg().unwrap())
    } else {
        format!("INT64_C({value})")
    }
}

fn append_matrix(source: &mut String) -> (usize, usize) {
    let values = boundary_values();
    source.push_str(
        "\ntypedef struct FixtureBinary { unsigned operation; int64_t left,right; uint32_t status; int64_t value; } FixtureBinary;\nstatic const FixtureBinary fixture_binary[] = {\n",
    );
    let mut binary_count = 0;
    for left in &values {
        for right in &values {
            for operation in 0..5 {
                let (status, value) = expected_binary(operation, *left, *right);
                writeln!(
                    source,
                    "  {{{operation}u,{},{},{status}u,{}}},",
                    c_integer(*left),
                    c_integer(*right),
                    c_integer(value),
                )
                .unwrap();
                binary_count += 1;
            }
        }
    }
    // These cases state negative quotient/remainder semantics explicitly in
    // addition to the full sign and boundary cross-product above.
    for (left, right) in [(-7, 3), (7, -3), (-7, -3)] {
        for operation in [3, 4] {
            let (status, value) = expected_binary(operation, left, right);
            assert_eq!(status, OK);
            writeln!(
                source,
                "  {{{operation}u,{},{},{status}u,{}}},",
                c_integer(left),
                c_integer(right),
                c_integer(value),
            )
            .unwrap();
            binary_count += 1;
        }
    }
    source.push_str(
        "};\ntypedef struct FixtureUnary { int64_t input; uint32_t status; int64_t value; } FixtureUnary;\nstatic const FixtureUnary fixture_unary[] = {\n",
    );
    for value in &values {
        let (status, result) = value
            .checked_neg()
            .map_or((OVERFLOW, 0), |value| (OK, value));
        writeln!(
            source,
            "  {{{},{status}u,{}}},",
            c_integer(*value),
            c_integer(result),
        )
        .unwrap();
    }
    source.push_str("};\n");
    (binary_count, values.len())
}

#[test]
fn native_task_integer_helpers_match_checked_i64_without_output_corruption_in_c() {
    let generated = generated_helpers();
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let mut source = generated.replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    let (binary_count, unary_count) = append_matrix(&mut source);
    assert_eq!(binary_count, 2_651);
    assert_eq!(unary_count, 23);
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-checked-integer");
    let c_file = directory.path().join("checked-integer.c");
    fs::write(&c_file, source).expect("write emitted integer helper fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "checked-integer") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the emitted integer arithmetic matrix"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before native execution");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("integer arithmetic matrix must obey the native process watchdog");
    assert!(
        output.status.success(),
        "integer arithmetic fixture failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        format!("task-checked-integer-ok {binary_count} {unary_count}\n"),
    );
    assert!(output.stderr.is_empty());
}

const C_MAIN: &str = r#"
typedef struct FixtureGuarded {
  uint64_t before;
  int64_t output;
  uint64_t after;
} FixtureGuarded;
typedef uint32_t (*FixtureOperation)(int64_t,int64_t,int64_t*);
static const FixtureOperation fixture_operations[] = {
  ku_int_add, ku_int_sub, ku_int_mul, ku_int_div, ku_int_rem
};
static size_t fixture_case;
#define CHECK(condition) do { if (!(condition)) { \
  fprintf(stderr,"checked integer case %zu line %d: %s\n",fixture_case,__LINE__,#condition); \
  return 1; \
} } while (0)
int main(void) {
  CHECK(KU_INT_OK==0u && KU_INT_OVERFLOW==1u && KU_INT_DIV_ZERO==2u);
  size_t binaries=sizeof(fixture_binary)/sizeof(fixture_binary[0]);
  size_t unaries=sizeof(fixture_unary)/sizeof(fixture_unary[0]);
  CHECK(binaries==2651u && unaries==23u);
  for (size_t i=0;i<binaries;i++) {
    fixture_case=i;
    const FixtureBinary* test=&fixture_binary[i];
    CHECK(test->operation<sizeof(fixture_operations)/sizeof(fixture_operations[0]));
    uint64_t before=UINT64_C(0x8A7B6C5D4E3F2010)+(uint64_t)i;
    uint64_t after=UINT64_C(0x123456789ABCDEF0)-(uint64_t)i;
    int64_t sentinel=INT64_C(0x13579BDF02468ACE)-(int64_t)i;
    FixtureGuarded guarded={before,sentinel,after};
    /* Force actual runtime operands even under an optimizing/sanitizer build;
     * the Rust-generated expectations must not fold into the C operation. */
    volatile int64_t left=test->left, right=test->right;
    uint32_t status=fixture_operations[test->operation](left,right,&guarded.output);
    CHECK(status==test->status);
    CHECK(guarded.before==before && guarded.after==after);
    CHECK(guarded.output==(status==KU_INT_OK ? test->value : sentinel));
  }
  for (size_t i=0;i<unaries;i++) {
    fixture_case=binaries+i;
    const FixtureUnary* test=&fixture_unary[i];
    uint64_t before=UINT64_C(0xFEDCBA9876543210)-(uint64_t)i;
    uint64_t after=UINT64_C(0x0123456789ABCDEF)+(uint64_t)i;
    int64_t sentinel=-INT64_C(0x13579BDF02468ACE)+(int64_t)i;
    FixtureGuarded guarded={before,sentinel,after};
    volatile int64_t input=test->input;
    uint32_t status=ku_int_neg(input,&guarded.output);
    CHECK(status==test->status);
    CHECK(guarded.before==before && guarded.after==after);
    CHECK(guarded.output==(status==KU_INT_OK ? test->value : sentinel));
  }
  printf("task-checked-integer-ok %zu %zu\n",binaries,unaries);
  return 0;
}
"#;

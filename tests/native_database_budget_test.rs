//! Process totals stay shared across database clients and event-loop threads.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::fs;
use std::process::Command;

use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};

#[test]
fn database_process_reservations_and_retained_inputs_are_bounded() {
    let directory = TempDir::new("database-budget");
    let source = directory.path().join("database-budget.c");
    fs::write(
        &source,
        format!(
            "#include <stdio.h>\n#include <string.h>\n#include <errno.h>\n#include <time.h>\n{}\n{}\n{}",
            include_str!("../src/backend/c_database_budget.c"),
            NATIVE_THREAD_LIFECYCLE_HARNESS,
            r#"
#define CHECK(condition) do { if (!(condition)) { fprintf(stderr,"check failed at %d\n",__LINE__); return 1; } } while (0)
typedef struct Fixture { KuTestEvent admitted, release; size_t charged; } Fixture;
static int hold_operation(void* raw) {
  Fixture* fixture = (Fixture*)raw;
  if (!ku_database_operation_begin(448, &fixture->charged)) return 1;
  if (!ku_test_event_set(&fixture->admitted)) return 2;
  if (!ku_test_event_wait(&fixture->release, 5000)) return 3;
  ku_database_operation_end(fixture->charged);
  return 0;
}
static int cycle_reservations(void* raw) {
  (void)raw;
  for (size_t index = 0; index < 2000; index++) {
    KuDatabaseReservation reservation = {0};
    if (!ku_database_budget_reserve(&reservation, 1, 1)) return 1;
    size_t charge = 0;
    if (!ku_database_operation_begin(16, &charge)) return 2;
    ku_database_operation_end(charge);
    ku_database_budget_release(&reservation);
  }
  return 0;
}
int main(void) {
  KuDatabaseBudgetSnapshot snapshot = ku_database_budget_snapshot();
  if (!snapshot.config_valid) {
    KuDatabaseReservation rejected = {0}; size_t charge = 0;
    CHECK(!ku_database_budget_reserve(&rejected, 1, 0));
    CHECK(!ku_database_operation_begin(0, &charge));
    CHECK(!charge && !rejected.connections);
    puts("invalid database budget rejected"); return 0;
  }
  CHECK(snapshot.max_connections == 8 && snapshot.max_waiters == 16);
  CHECK(snapshot.max_pending_bytes == 2048);
  KuDatabaseReservation pg = {0}, mysql = {0}, redis = {0};
  CHECK(ku_database_budget_reserve(&pg, 4, 8));
  CHECK(ku_database_budget_reserve(&mysql, 4, 8));
  CHECK(!ku_database_budget_reserve(&redis, 1, 0));
  ku_database_budget_release(&pg);
  CHECK(!ku_database_budget_reserve(&redis, 1, 9));
  CHECK(ku_database_budget_reserve(&redis, 4, 8));
  snapshot = ku_database_budget_snapshot();
  CHECK(snapshot.reserved_connections == 8 && snapshot.reserved_waiters == 16);
  ku_database_budget_release(&redis); ku_database_budget_release(&mysql);
  /* An HTTP deadline ending must not refund another thread's live operation. */
  Fixture fixtures[4] = {0}; KuTestThread threads[4] = {0};
  for (size_t index = 0; index < 4; index++) {
    CHECK(ku_test_event_init(&fixtures[index].admitted));
    CHECK(ku_test_event_init(&fixtures[index].release));
    CHECK(ku_test_thread_start(&threads[index], hold_operation, &fixtures[index]));
    CHECK(ku_test_event_wait(&fixtures[index].admitted, 5000));
  }
  size_t rejected_charge = 0;
  CHECK(!ku_database_operation_begin(0, &rejected_charge));
  CHECK(!ku_database_operation_begin(SIZE_MAX, &rejected_charge));
  snapshot = ku_database_budget_snapshot();
  CHECK(snapshot.pending_operations == 4 && snapshot.pending_bytes == 2048);
  for (size_t index = 0; index < 4; index++) {
    CHECK(ku_test_event_set(&fixtures[index].release));
    CHECK(ku_test_thread_join(&threads[index], 5000));
    CHECK(!threads[index].outcome);
    CHECK(ku_test_event_destroy(&fixtures[index].admitted));
    CHECK(ku_test_event_destroy(&fixtures[index].release));
  }
  for (size_t index = 0; index < 4; index++)
    CHECK(ku_test_thread_start(&threads[index], cycle_reservations, NULL));
  for (size_t index = 0; index < 4; index++) {
    CHECK(ku_test_thread_join(&threads[index], 5000)); CHECK(!threads[index].outcome);
  }
  ku_database_budget_record_unknown();
  snapshot = ku_database_budget_snapshot();
  CHECK(!snapshot.reserved_connections && !snapshot.reserved_waiters);
  CHECK(!snapshot.pending_operations && !snapshot.pending_bytes);
  CHECK(snapshot.peak_pending_bytes == 2048 && snapshot.rejections == 4);
  CHECK(snapshot.unknown_results == 1);
  puts("database process budget closed loop"); return 0;
}
"#,
        ),
    )
    .expect("write budget regression harness");
    let Some(executable) = compile_harness(directory.path(), &source, "database-budget") else {
        return;
    };
    for value in ["2048", "0", "-1", "2048x", "184467440737095516160"] {
        let mut command = Command::new(&executable);
        command
            .env("KU_DB_MAX_CONNECTIONS", "8")
            .env("KU_DB_MAX_WAITERS", "16")
            .env("KU_DB_MAX_PENDING_BYTES", value);
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .expect("bounded database budget harness");
        assert!(
            output.status.success(),
            "budget harness failed for {value}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            if value == "2048" {
                "database process budget closed loop\n"
            } else {
                "invalid database budget rejected\n"
            }
        );
    }
}

//! Explicit CI-only detector self-test, not a Task correctness or performance result.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{env, fs, panic, process::Command};

#[test]
#[ignore = "requires the explicit Linux Task TSan job and working instrumented runtime"]
fn native_task_tsan_instrumentation_and_runtime_probe() {
    assert!(cfg!(target_os = "linux"));
    assert_eq!(env::var("KU_NATIVE_REQUIRE_TSAN").as_deref(), Ok("1"));
    assert_eq!(
        env::var("TSAN_OPTIONS").as_deref(),
        Ok("halt_on_error=1:exitcode=66")
    );
    if let Ok(expected) = env::var("KU_NATIVE_TSAN_EXPECT_REJECTION") {
        let directory = TempDir::new("tsan-compiler-contract");
        let c_file = directory.path().join("probe.c");
        fs::write(&c_file, "int main(void) { return 0; }\n").unwrap();
        let rejected = panic::catch_unwind(|| compile_harness(directory.path(), &c_file, "probe"))
            .expect_err("invalid required compiler must not fall back or report a skip");
        let message = rejected
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| rejected.downcast_ref::<&str>().copied())
            .expect("compiler rejection must preserve its diagnostic");
        assert!(
            message.contains(&expected),
            "unexpected rejection: {message}"
        );
        assert!(!directory.path().join("probe").exists());
        return;
    }
    // These child invocations never compile: test the actual harness's refusal
    // to fall back, or accept flags/extra compiler and linker inputs via KU_CC.
    let missing = TempDir::new("tsan-missing-compiler");
    for (compiler, expected) in [
        (
            missing
                .path()
                .join("does-not-exist")
                .to_string_lossy()
                .into_owned(),
            "could not be spawned",
        ),
        (
            format!(
                "{} -include /dev/null -fno-sanitize=thread extra.o",
                env::var("KU_CC").unwrap()
            ),
            "must be one executable, without extra flags or inputs",
        ),
    ] {
        let mut child = Command::new(env::current_exe().unwrap());
        child
            .args([
                "native_task_tsan_instrumentation_and_runtime_probe",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("KU_CC", compiler)
            .env("KU_NATIVE_TSAN_EXPECT_REJECTION", expected);
        let output = run_bounded(&mut child, RUN_TIMEOUT, RUN_LIMITS)
            .expect("compiler contract child must remain bounded");
        assert!(
            output.status.success(),
            "required compiler contract failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout)
            .contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    }
    for (stem, storage, write, read, marker) in [
        (
            "tsan_clean",
            "static _Atomic int probe_value;",
            "atomic_store_explicit(&probe_value, value, memory_order_relaxed);",
            "atomic_load_explicit(&probe_value, memory_order_relaxed)",
            "tsan-clean-ok",
        ),
        (
            "tsan_race",
            "static volatile int probe_value;",
            "probe_value = value;",
            "probe_value",
            "tsan-race-was-not-detected",
        ),
    ] {
        let source = PROBE
            .replace("@STORAGE@", storage)
            .replace("@WRITE@", write)
            .replace("@READ@", read)
            .replace("@MARKER@", marker);
        let directory = TempDir::new(stem);
        let c_file = directory.path().join(format!("{stem}.c"));
        fs::write(&c_file, source).unwrap();
        let executable = compile_harness(directory.path(), &c_file, stem)
            .expect("required TSan probe cannot skip a missing compiler");
        fs::remove_file(c_file).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .expect("TSan probe must execute within the unchanged bounded runner");
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stem == "tsan_clean" {
            assert!(
                output.status.success(),
                "clean TSan probe failed:\n{stderr}"
            );
            assert_eq!(output.stdout, b"tsan-clean-ok\n");
            assert!(stderr.is_empty(), "clean TSan stderr:\n{stderr}");
        } else {
            assert_eq!(
                output.status.code(),
                Some(66),
                "race probe stderr:\n{stderr}"
            );
            assert!(
                stderr.contains("WARNING: ThreadSanitizer: data race"),
                "a crash, unavailable runtime, mapping failure or timeout is not a race self-test:\n{stderr}"
            );
            assert!(
                stderr.contains("ku_tsan_probe_writer"),
                "require the actual instrumented canary access in the report:\n{stderr}"
            );
            println!("expected isolated TSan canary diagnostic:\n{stderr}");
        }
    }
}

const PROBE: &str = r#"
#define _POSIX_C_SOURCE 200809L
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#ifndef __has_feature
#define __has_feature(feature) 0
#endif
#if !__has_feature(thread_sanitizer)
#error Actual probe translation unit was not instrumented
#endif
@STORAGE@
static pthread_barrier_t probe_start;
static void* ku_tsan_probe_writer(void* raw) {
  int value = (int)(intptr_t)raw;
  int status = pthread_barrier_wait(&probe_start);
  if (status != 0 && status != PTHREAD_BARRIER_SERIAL_THREAD) abort();
  @WRITE@
  return NULL;
}
int main(void) {
  pthread_t a, b;
  if (pthread_barrier_init(&probe_start, NULL, 2)) return 2;
  if (pthread_create(&a, NULL, ku_tsan_probe_writer, (void*)(intptr_t)1)) return 3;
  if (pthread_create(&b, NULL, ku_tsan_probe_writer, (void*)(intptr_t)2)) return 4;
  if (pthread_join(a, NULL) || pthread_join(b, NULL)) return 5;
  if (pthread_barrier_destroy(&probe_start)) return 6;
  int observed = @READ@;
  if (observed != 1 && observed != 2) return 7;
  puts("@MARKER@");
  return 0;
}
"#;

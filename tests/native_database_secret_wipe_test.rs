//! Generated-C contracts for database-owned secret buffers.
//!
//! These tests deliberately do not claim that libpq/libmysqlclient scrub their
//! opaque allocations, and they do not mutate the caller-owned Ku config.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::fs;
use std::process::Command;

use native_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

fn fixture() -> &'static str {
    r#"import pg from "std.pg"
import mysql from "std.mysql"
import redis from "std.redis"

fn main(): null! {
    pg_client = pg.client({ conninfo: "hostaddr=127.0.0.1 password=pg-secret" })?
    pg_client.close()
    mysql_client = mysql.client({
        host: "127.0.0.1",
        user: "tester",
        password: "mysql-secret",
        database: "fixture"
    })?
    mysql_client.close()
    redis_client = redis.client({
        host: "127.0.0.1",
        username: "tester",
        password: "redis-secret"
    })?
    redis_client.close()
    return ok(null)
}
"#
}

fn function<'a>(generated: &'a str, signature: &str) -> &'a str {
    let start = generated
        .find(signature)
        .unwrap_or_else(|| panic!("generated C is missing {signature}"));
    let body = &generated[start..];
    let open = body
        .find('{')
        .unwrap_or_else(|| panic!("generated C function has no body: {signature}"));
    let mut depth = 0usize;
    for (offset, byte) in body.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &body[..open + offset + 1];
                }
            }
            _ => {}
        }
    }
    panic!("generated C function is unterminated: {signature}");
}

fn assert_order(body: &str, labels: &[&str]) {
    let mut previous = 0usize;
    for (index, label) in labels.iter().enumerate() {
        let position = body
            .find(label)
            .unwrap_or_else(|| panic!("missing `{label}` in:\n{body}"));
        if index != 0 {
            assert!(
                previous < position,
                "`{}` must precede `{label}` in:\n{body}",
                labels[index - 1]
            );
        }
        previous = position;
    }
}

#[test]
fn native_database_secrets_are_scrubbed_before_fail_stop_and_freed_after_it() {
    let directory = TempDir::new("database-secret-order");
    let generated = emit_c(directory.path(), fixture());

    let pg_dispose = function(
        &generated,
        "static void ku_pg_client_dispose(KuPgClient* p) {",
    );
    assert_order(
        pg_dispose,
        &[
            "ku_pg_wipe_secret(p->conninfo, p->conninfo_len)",
            "ku_pg_sync_destroy(&p->lock, &p->cv)",
            "free(p->conninfo)",
        ],
    );
    let pg_open = function(&generated, "static KuResult_pg_client ku_pg_client_open(");
    let initial_failure = pg_open
        .split_once("if (!initial) {")
        .expect("PostgreSQL initial connection failure path")
        .1;
    assert_order(
        initial_failure,
        &[
            "ku_pg_wipe_secret(p->conninfo, p->conninfo_len)",
            "ku_pg_sync_destroy(&p->lock, &p->cv)",
            "free(p->conninfo)",
        ],
    );

    let mysql_destroy = function(
        &generated,
        "static void ku_mysql_client_destroy(KuMysqlClient* client)",
    );
    assert_order(
        mysql_destroy,
        &[
            "ku_mysql_secure_wipe(client->password, client->password_len)",
            "ku_mysql_sync_destroy(client)",
            "ku_mysql_client_free_fields(client)",
        ],
    );
    let mysql_free = function(
        &generated,
        "static void ku_mysql_secure_free(char* value, size_t len)",
    );
    assert_order(
        mysql_free,
        &["ku_mysql_secure_wipe(value, len)", "ku_mysql_free(value)"],
    );

    let redis_dispose = function(&generated, "static void ku_redis_client_free_unpublished(");
    let redis_sync = redis_dispose
        .find("ku_redis_pool_sync_destroy(&client->sync)")
        .expect("Redis synchronization destroy");
    let redis_connection_destroy = redis_dispose
        .find("ku_redis_connection_destroy(client->idle[index])")
        .expect("Redis connection synchronization destroy path");
    for wipe in [
        "ku_redis_secure_wipe(&client->username)",
        "ku_redis_secure_wipe(&client->password)",
    ] {
        assert!(
            redis_dispose.find(wipe).expect("Redis credential wipe") < redis_connection_destroy,
            "{wipe} must precede every fail-stop synchronization destructor"
        );
    }
    assert!(redis_connection_destroy < redis_sync);
    assert!(
        redis_sync
            < redis_dispose
                .find("ku_string_drop(&client->password)")
                .expect("Redis password free"),
        "Redis must keep credential allocations live through synchronization destruction"
    );

    for helper in [
        function(
            &generated,
            "static void ku_pg_wipe_secret(void* pointer, size_t len)",
        ),
        function(
            &generated,
            "static void ku_mysql_secure_wipe(char* value, size_t len)",
        ),
        function(
            &generated,
            "static void ku_redis_secure_wipe_bytes(void* pointer, size_t len)",
        ),
    ] {
        assert!(
            helper.contains("volatile"),
            "secret wipe must not be elided: {helper}"
        );
    }
}

#[test]
fn native_redis_auth_scrubs_reflected_stack_and_heap_bytes() {
    let directory = TempDir::new("redis-auth-secret-order");
    let generated = emit_c(directory.path(), fixture());
    let auth_reply = function(
        &generated,
        "static KuResult_null ku_redis_simple_expected_locked(",
    );
    let read = auth_reply
        .find("ku_redis_read_line(r, line, sizeof(line), &len)")
        .expect("Redis AUTH reply read");
    let stack_wipe = auth_reply
        .find("ku_redis_secure_wipe_bytes(line, sizeof(line))")
        .expect("Redis AUTH stack wipe");
    let heap_wipe = auth_reply
        .find("ku_redis_secure_wipe_bytes(r->read_buffer, sizeof(r->read_buffer))")
        .expect("Redis AUTH heap read-buffer wipe");
    let result_return = auth_reply
        .rfind("return result")
        .expect("Redis AUTH result return");
    assert!(
        read < stack_wipe && stack_wipe < heap_wipe && heap_wipe < result_return,
        "Redis must scrub both reflected copies before returning the redacted AUTH result"
    );

    let destroy = function(
        &generated,
        "static void ku_redis_connection_destroy(KuRedis* connection)",
    );
    assert_order(
        destroy,
        &[
            "ku_redis_poison(connection)",
            "ku_redis_secure_wipe_bytes(",
            "ku_redis_gate_destroy(&connection->command_gate)",
            "free(connection)",
        ],
    );
}

#[test]
fn native_database_wipe_helpers_survive_normal_and_fail_stop_exit() {
    let directory = TempDir::new("database-secret-runtime");
    let generated = emit_c(directory.path(), fixture());
    let pg_wipe = function(
        &generated,
        "static void ku_pg_wipe_secret(void* pointer, size_t len)",
    );
    let mysql_wipe = function(
        &generated,
        "static void ku_mysql_secure_wipe(char* value, size_t len)",
    );
    let redis_wipe = function(
        &generated,
        "static void ku_redis_secure_wipe_bytes(void* pointer, size_t len)",
    );
    let source = directory.path().join("database-secret-runtime.c");
    fs::write(
        &source,
        format!(
            r#"#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

{pg_wipe}
{mysql_wipe}
{redis_wipe}

static unsigned char pg_secret[17];
static char mysql_secret[20];
static unsigned char redis_secret[20];

static int all_zero(const void* pointer, size_t len) {{
  const unsigned char* bytes = (const unsigned char*)pointer;
  for (size_t index = 0; index < len; index++) if (bytes[index] != 0) return 0;
  return 1;
}}

static void verify_exit_wipe(void) {{
  if (all_zero(pg_secret, sizeof(pg_secret))
      && all_zero(mysql_secret, sizeof(mysql_secret))
      && all_zero(redis_secret, sizeof(redis_secret))) return;
  fputs("secret bytes survived fail-stop\n", stderr);
  _Exit(99);
}}

int main(int argc, char** argv) {{
  if (argc != 2) return 64;
  memcpy(pg_secret, "pg-driver-secret", 16);
  memcpy(mysql_secret, "mysql-driver-secret", 19);
  memcpy(redis_secret, "redis-driver-secret", 19);
  if (atexit(verify_exit_wipe) != 0) return 65;
  ku_pg_wipe_secret(pg_secret, sizeof(pg_secret));
  ku_mysql_secure_wipe(mysql_secret, sizeof(mysql_secret));
  ku_redis_secure_wipe_bytes(redis_secret, sizeof(redis_secret));
  if (strcmp(argv[1], "destroy-failure") == 0) exit(7);
  if (strcmp(argv[1], "normal") != 0) return 66;
  if (!all_zero(pg_secret, sizeof(pg_secret))
      || !all_zero(mysql_secret, sizeof(mysql_secret))
      || !all_zero(redis_secret, sizeof(redis_secret))) return 67;
  puts("database secrets wiped");
  return 0;
}}
"#
        ),
    )
    .expect("write database secret runtime harness");
    let Some(executable) = compile_harness(directory.path(), &source, "database-secret-runtime")
    else {
        eprintln!("skip: no C compiler available for database secret runtime test");
        return;
    };

    let mut normal = Command::new(&executable);
    normal.current_dir(directory.path()).arg("normal");
    let normal = run_bounded(&mut normal, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("database secret normal case was not bounded: {error}"));
    assert!(normal.status.success(), "normal wipe failed: {normal:?}");
    assert_eq!(
        String::from_utf8_lossy(&normal.stdout).replace('\r', ""),
        "database secrets wiped\n"
    );
    assert!(normal.stderr.is_empty());

    let mut failure = Command::new(executable);
    failure.current_dir(directory.path()).arg("destroy-failure");
    let failure = run_bounded(&mut failure, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("database secret fail-stop case was not bounded: {error}"));
    assert_eq!(failure.status.code(), Some(7));
    assert!(failure.stdout.is_empty());
    assert!(failure.stderr.is_empty());
}

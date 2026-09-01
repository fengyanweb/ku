#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use native_pg_harness::{emit_c, TempDir};

// Exact C signatures for every Ku-reachable database receiver, result and
// ownership helper. Pool acquire/release and transport helpers are intentionally
// excluded because no Ku intrinsic can call them directly.
const PRIVATE_DATABASE_HELPERS: &[&str] = &[
    "static KuResult_pg_client ku_pg_client(",
    "static KuResult_pg_result ku_pg_client_query(",
    "static int64_t ku_pg_rows(",
    "static int64_t ku_pg_cols(",
    "static KuResult_str ku_pg_value(",
    "static KuResult_bool ku_pg_is_null(",
    "static uint8_t ku_pg_client_close(",
    "static KuPgClient* ku_move_pg_client(",
    "static void ku_drop_pg_client(",
    "static KuPgClient* ku_clone_pg_client(",
    "static KuPgResult* ku_move_pg_result(",
    "static void ku_drop_pg_result(",
    "static KuPgResult* ku_clone_pg_result(",
    "static KuResult_mysql_client ku_mysql_client_new(",
    "static KuResult_mysql_result ku_mysql_client_query(",
    "static KuResult_int ku_mysql_client_execute(",
    "static int64_t ku_mysql_result_rows(",
    "static int64_t ku_mysql_result_cols(",
    "static KuResult_str ku_mysql_result_value(",
    "static KuResult_bool ku_mysql_result_is_null(",
    "static uint8_t ku_mysql_client_close(",
    "static KuMysqlClient* ku_move_mysql_client(",
    "static void ku_drop_mysql_client(",
    "static KuMysqlClient* ku_clone_mysql_client(",
    "static KuMysqlResult* ku_move_mysql_result(",
    "static void ku_drop_mysql_result(",
    "static KuMysqlResult* ku_clone_mysql_result(",
    "static KuResult_redis_client ku_redis_client(",
    "static KuResult_null ku_redis_ping(",
    "static KuResult_null ku_redis_set(",
    "static KuResult_str ku_redis_get(",
    "static KuResult_int ku_redis_del(",
    "static KuResult_bool ku_redis_exists(",
    "static uint8_t ku_redis_close(",
    "static KuRedisClient* ku_move_redis_client(",
    "static void ku_drop_redis_client(",
    "static KuRedisClient* ku_clone_redis_client(",
];

fn database_fixture() -> &'static str {
    r#"import pg from "std.pg"
import mysql from "std.mysql"
import redis from "std.redis"

fn main(): null! {
    postgres = pg.client({ conninfo: "host=127.0.0.1" })?
    pg_result = postgres.query("SELECT $1", ["value"])?
    pg_rows = pg_result.rows()
    pg_cols = pg_result.cols()
    pg_value = pg_result.value(0, 0)?
    pg_null = pg_result.is_null(0, 0)?
    postgres.close()

    maria = mysql.client({
        host: "127.0.0.1", user: "user", password: "password", database: "db"
    })?
    mysql_result = maria.query("SELECT ?", ["value"])?
    mysql_rows = mysql_result.rows()
    mysql_cols = mysql_result.cols()
    mysql_value = mysql_result.value(0, 0)?
    mysql_null = mysql_result.is_null(0, 0)?
    affected = maria.execute("UPDATE values SET value = ?", ["value"])?
    maria.close()

    cache = redis.client({ host: "127.0.0.1" })?
    cache.ping()?
    cache.set("key", "value")?
    cached = cache.get("key")?
    present = cache.exists("key")?
    deleted = cache.del("key")?
    cache.close()
    return ok(null)
}
"#
}

#[test]
fn native_database_helpers_are_private_and_safe_close_moves_each_client() {
    let directory = TempDir::new("database-lifetime-contract");
    let generated = emit_c(directory.path(), database_fixture());

    for &signature in PRIVATE_DATABASE_HELPERS {
        assert!(
            generated.contains(signature),
            "database helper must remain translation-unit private: {signature}"
        );
        let external_signature = signature
            .strip_prefix("static ")
            .expect("private helper signatures start with static");
        assert!(
            !generated
                .lines()
                .any(|line| line.trim_start().starts_with(external_signature)),
            "raw database helper escaped as an external C symbol: {external_signature}"
        );
    }

    for moved_close in [
        "ku_pg_client_close(ku_move_pg_client(&",
        "ku_mysql_client_close(ku_move_mysql_client(&",
        "ku_redis_close(ku_move_redis_client(&",
    ] {
        assert!(
            generated.contains(moved_close),
            "safe Ku close must consume and clear its unique client owner: {moved_close}"
        );
    }
}

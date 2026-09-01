#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use native_pg_harness::{emit_c, TempDir};

#[test]
fn native_database_helpers_are_private_and_safe_close_moves_each_client() {
    let directory = TempDir::new("database-lifetime-contract");
    let generated = emit_c(
        directory.path(),
        r#"import pg from "std.pg"
import mysql from "std.mysql"
import redis from "std.redis"

fn main(): null! {
    postgres = pg.client({ conninfo: "host=127.0.0.1" })?
    postgres.close()
    maria = mysql.client({
        host: "127.0.0.1", user: "user", password: "password", database: "db"
    })?
    maria.close()
    cache = redis.client({ host: "127.0.0.1" })?
    cache.close()
    return ok(null)
}
"#,
    );

    for private_helper in [
        "static uint8_t ku_pg_client_close(",
        "static uint8_t ku_mysql_client_close(",
        "static uint8_t ku_redis_close(",
    ] {
        assert!(
            generated.contains(private_helper),
            "database runtime helper must remain translation-unit private: {private_helper}"
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

    for exported_helper in [
        "\nuint8_t ku_pg_client_close(",
        "\nuint8_t ku_mysql_client_close(",
        "\nuint8_t ku_redis_close(",
    ] {
        assert!(
            !generated.contains(exported_helper),
            "raw database helpers are not a supported external C ABI"
        );
    }
}

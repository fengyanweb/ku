//! Opt-in Oracle MySQL acceptance against mysql-loopback-fixture.py only.
//!
//! This is not a service discovery test: absent/foreign configuration fails
//! before compilation or connection. Credentials remain in the private fixture
//! directory, never in generated Ku/C source or compiler diagnostics. Native
//! Task cancellation and concurrent close are separate, unclaimed contracts.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use native_harness::{run_bounded, OutputLimits, TempDir};

const CONFIG_LIMIT: u64 = 8192;
const PREFIX: &str = "mysql-loopback-8.0.29-";

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// A deliberately narrow parser for the fixture's five ASCII scalar fields,
// not a general JSON parser. No escapes, duplicates, extra fields, nested
// values or alternative numeric spellings are accepted. No new dependency.
fn private_config_password(text: &str) -> Option<&str> {
    if text.len() > CONFIG_LIMIT as usize {
        return None;
    }
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut fields = BTreeMap::new();
    for item in inner.split(',') {
        if fields.len() == 5 {
            return None;
        }
        let (key, value) = item.split_once(':')?;
        let key = key.trim().strip_prefix('"')?.strip_suffix('"')?;
        if !["host", "port", "user", "password", "database"].contains(&key)
            || fields.insert(key, value.trim()).is_some()
        {
            return None;
        }
    }
    // host, port, user, password and database are the complete config schema.
    if fields.len() != 5 || fields.get("host")? != &"\"127.0.0.1\"" {
        return None;
    }
    let port_text = *fields.get("port")?;
    let port: u16 = port_text.parse().ok()?;
    if port < 1024 || port.to_string() != port_text {
        return None;
    }
    let string = |key| fields.get(key)?.strip_prefix('"')?.strip_suffix('"');
    if !lower_hex(string("user")?.strip_prefix("ku_test_")?, 16)
        || !lower_hex(string("database")?.strip_prefix("ku_db_")?, 16)
    {
        return None;
    }
    let password = string("password")?;
    lower_hex(password, 64).then_some(password)
}

fn plain(path: &Path, directory: bool) {
    assert!(path.is_absolute(), "private fixture paths must be absolute");
    assert!(
        path.components()
            .all(|part| !matches!(part, Component::ParentDir | Component::CurDir)),
        "private fixture paths must not contain traversal"
    );
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).expect("inspect private fixture path");
        assert!(
            !metadata.file_type().is_symlink(),
            "private fixture paths must not contain symlinks"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            assert_eq!(
                metadata.file_attributes() & 0x400,
                0,
                "private fixture paths must not contain reparse points"
            );
        }
    }
    let metadata = fs::metadata(path).expect("inspect private fixture entry");
    assert!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "private fixture entry has the wrong type"
    );
}

fn private_fixture() -> (PathBuf, String) {
    assert!(cfg!(windows), "this fixture currently requires Windows");
    let config = PathBuf::from(
        env::var_os("KU_MYSQL_TEST_CONFIG_FILE")
            .expect("run this ignored test through mysql-loopback-fixture.py verify"),
    );
    assert!(
        !config.to_string_lossy().starts_with("\\\\"),
        "network paths are not private fixtures"
    );
    plain(&config, false);
    assert_eq!(config.file_name().and_then(|v| v.to_str()), Some("db.json"));
    let root = config.parent().expect("private fixture directory");
    let name = root.file_name().and_then(|v| v.to_str()).unwrap_or("");
    assert!(
        name.strip_prefix(PREFIX)
            .is_some_and(|suffix| lower_hex(suffix, 32)),
        "configuration is not a newly named MySQL fixture"
    );
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    plain(&target, true);
    assert_eq!(
        root.parent().and_then(|v| v.canonicalize().ok()),
        target.canonicalize().ok(),
        "configuration is outside the private fixture root"
    );
    // These markers are created by the owning supervisor, not by this test.
    for name in [
        "operation.lock",
        "server.active",
        "server.pid",
        "fixture.json",
    ] {
        plain(&root.join(name), false);
    }
    for (variable, leaf) in [("KU_MYSQL_LIB", "lib"), ("KU_MYSQL_INCLUDE", "include")] {
        let expected = root.join("portable").join(leaf);
        plain(&expected, true);
        let actual = PathBuf::from(env::var_os(variable).expect("private MySQL SDK path"));
        plain(&actual, true);
        assert_eq!(
            actual.canonicalize().ok(),
            expected.canonicalize().ok(),
            "MySQL SDK must come from this private fixture"
        );
    }
    let mut bytes = Vec::new();
    File::open(&config)
        .expect("open private fixture configuration")
        .take(CONFIG_LIMIT + 1)
        .read_to_end(&mut bytes)
        .expect("read bounded private fixture configuration");
    let text =
        String::from_utf8(bytes).unwrap_or_else(|_| panic!("fixture configuration must be UTF-8"));
    let password = private_config_password(&text)
        .expect("invalid private loopback configuration; no connection attempted")
        .to_owned();
    (root.to_path_buf(), password)
}

#[test]
#[ignore = "requires a newly created private mysql-loopback-fixture.py instance"]
fn native_mysql_client_live_loopback_roundtrip() {
    let (root, password) = private_fixture();
    let directory = TempDir::new("mysql-live");
    let source = directory.path().join("main.ku");
    let executable = directory.path().join("mysql-live.exe");
    // Native fs paths are source-relative, not launch-cwd-relative. Only the
    // validated private config locator is embedded; the credential bytes are
    // loaded at run time. A source/data relocation test belongs elsewhere.
    let config_path = root.join("db.json").to_string_lossy().replace('\\', "/");
    assert!(
        !config_path.chars().any(char::is_control),
        "private fixture path contains control characters"
    );
    let rendered = LIVE_SOURCE.replace("__KU_PRIVATE_CONFIG__", &config_path.replace('"', "\\\""));
    fs::write(&source, rendered).expect("write non-secret MySQL native fixture source");
    let ku = PathBuf::from(env::var_os("KU_BIN").expect("fixture must supply the Ku CLI"));
    plain(&ku, false);
    let mut build = Command::new(ku);
    build
        .current_dir(directory.path())
        .args(["build", "--native", "main.ku", "-o"])
        .arg(&executable);
    let built = run_bounded(
        &mut build,
        Duration::from_secs(90),
        OutputLimits::new(4 * 1024 * 1024, 8 * 1024 * 1024),
    )
    .unwrap_or_else(|_| panic!("MySQL native build exceeded its process/output bound"));
    assert!(
        built.status.success(),
        "MySQL native build failed:\n{}{}",
        String::from_utf8_lossy(&built.stdout).replace(&password, "<redacted>"),
        String::from_utf8_lossy(&built.stderr).replace(&password, "<redacted>")
    );
    let mut command = Command::new(executable);
    command.current_dir(&root);
    let output = run_bounded(
        &mut command,
        Duration::from_secs(20),
        OutputLimits::new(1024 * 1024, 2 * 1024 * 1024),
    )
    .unwrap_or_else(|_| panic!("MySQL live fixture exceeded its process/output bound"));
    assert!(
        output.status.success(),
        "MySQL live fixture failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout).replace(&password, "<redacted>"),
        String::from_utf8_lossy(&output.stderr).replace(&password, "<redacted>")
    );
    assert!(
        !output
            .stdout
            .windows(password.len())
            .any(|v| v == password.as_bytes())
            && !output
                .stderr
                .windows(password.len())
                .any(|v| v == password.as_bytes()),
        "MySQL runtime leaked fixture credentials"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "mysql live loopback closed loop\n"
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected MySQL runtime diagnostics"
    );
}

const LIVE_SOURCE: &str = r#"import mysql from "std.mysql"
import fs from "std.fs"
import json from "std.json"

fn main(): null! {
    value = json.parse(fs.read("__KU_PRIVATE_CONFIG__")?)?
    client = mysql.client({
        host: value["host"]?.as_str()?,
        port: value["port"]?.as_int()?,
        user: value["user"]?.as_str()?,
        password: value["password"]?.as_str()?,
        database: value["database"]?.as_str()?,
        max_connections: 1,
        max_waiters: 0,
        connect_timeout_ms: 1000,
        acquire_timeout_ms: 1000,
        query_timeout_ms: 5000
    })?
    initial = client.query("SELECT CONNECTION_ID()", [])?
    initial_id = initial.value(0, 0)?
    reused = client.query("SELECT CONNECTION_ID()", [])?
    if (reused.value(0, 0)? != initial_id) panic("single-slot pool did not reuse connection")

    client.execute("DROP TABLE IF EXISTS ku_live_values", [])?
    client.execute("CREATE TABLE ku_live_values (id INT PRIMARY KEY, value TEXT NOT NULL) CHARACTER SET utf8mb4", [])?
    payload = "x'; DROP TABLE ku_live_values; --"
    inserted = client.execute("INSERT INTO ku_live_values (id, value) VALUES (1, ?), (2, ?), (3, ?)", [payload.clone(), "", "你好🌍"])?
    if (inserted != 3) panic("prepared insert affected rows")
    values = client.query("SELECT value FROM ku_live_values ORDER BY id", [])?
    if (values.rows() != 3 || values.cols() != 1) panic("prepared result shape")
    if (values.value(0, 0)? != payload) panic("prepared parameter injection")
    if (values.value(1, 0)? != "") panic("empty string result")
    if (values.value(2, 0)? != "你好🌍") panic("UTF-8 result")
    nullable = client.query("SELECT NULL, CAST(? AS CHAR CHARACTER SET utf8mb4), REPEAT('z', 300)", [""])?
    if (!nullable.is_null(0, 0)? || nullable.is_null(0, 1)?) panic("SQL NULL versus empty string")
    // Parenthesize Result unwrap before field access; ?. is optional access.
    if (nullable.value(0, 1)? != "" || (nullable.value(0, 2)?).byte_len() != 300) panic("empty or overflow fetch")
    try {
        invalid_null = nullable.value(0, 0)?
        panic("SQL NULL accepted as str")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "null_value") panic("NULL error contract")
    }
    try {
        invalid_index = nullable.value(1, 0)?
        panic("result bounds accepted")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "index_out_of_bounds") panic("bounds error contract")
    }
    try {
        invalid_count = client.query("SELECT ?", [])?
        panic("missing parameter accepted")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "parameter_count") panic("parameter error contract")
    }
    try {
        invalid_sql = client.query("SELECT value FROM ku_live_missing_table", [])?
        panic("missing table accepted")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "query_error") panic("query error contract")
    }
    try {
        client.execute("INSERT INTO ku_live_values (id, value) VALUES (1, ?)", ["duplicate"])?
        panic("duplicate primary key accepted")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "query_error") panic("execute error contract")
    }

    // A preflight rejection must neither transmit SET nor replace the slot.
    try {
        client.execute(" /* ordinary */ SET autocommit = 0", [])?
        panic("explicit session control accepted")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "session_state_unsupported") panic("session preflight contract")
    }
    try {
        rejected = client.query("/*!40101 SET autocommit = 0 */ SELECT 1", [])?
        panic("executable comment accepted")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "session_state_unsupported") panic("comment preflight contract")
    }
    after_rejection = client.query("SELECT CONNECTION_ID(), @@autocommit", [])?
    if (after_rejection.value(0, 0)? != initial_id || after_rejection.value(0, 1)? != "1") panic("preflight changed session")

    // Real post-execution user-variable contamination is cleared by reset.
    // This does NOT pretend to trigger the separate IN_TRANS/AUTOCOMMIT
    // post-policy error branch: that branch has deterministic fake-ABI tests.
    dirty = client.query("SELECT @ku_live_probe := CAST(? AS CHAR CHARACTER SET utf8mb4)", ["dirty"])?
    if (dirty.value(0, 0)? != "dirty") panic("session mutation did not execute")
    clean = client.query("SELECT @ku_live_probe IS NULL, CONNECTION_ID()", [])?
    if (clean.value(0, 0)? != "1" || clean.value(0, 1)? != initial_id) panic("reset did not clear session in reused slot")
    client.execute("DROP TABLE ku_live_values", [])?
    client.close()
    // Detached result ownership outlives the consumed client.
    if (values.value(0, 0)? != payload || values.value(2, 0)? != "你好🌍") panic("close invalidated detached results")

    // Blocking MySQL APIs currently have a soft deadline, not hard async
    // interruption. A finite two-second query plus the outer 20-second
    // process-tree watchdog bounds this check without promising sub-second IO.
    slow_client = mysql.client({
        host: value["host"]?.as_str()?,
        port: value["port"]?.as_int()?,
        user: value["user"]?.as_str()?,
        password: value["password"]?.as_str()?,
        database: value["database"]?.as_str()?,
        max_connections: 1,
        max_waiters: 0,
        connect_timeout_ms: 1000,
        acquire_timeout_ms: 1000,
        query_timeout_ms: 500
    })?
    before_timeout = slow_client.query("SELECT CONNECTION_ID()", [])?
    before_timeout_id = before_timeout.value(0, 0)?
    try {
        slow = slow_client.query("SELECT SLEEP(2)", [])?
        panic("slow query escaped deadline")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "execution_unknown") panic("uncertain timeout outcome contract")
    }
    recovered = slow_client.query("SELECT CONNECTION_ID(), @@autocommit", [])?
    if (recovered.value(0, 0)? == before_timeout_id || recovered.value(0, 1)? != "1") panic("timeout connection was reused")
    slow_client.close()
    if (recovered.rows() != 1) panic("timeout close invalidated result")
    println("mysql live loopback closed loop")
    return ok(null)
}
"#;

#[test]
fn native_mysql_live_config_requires_exact_private_scalars() {
    let valid = format!(
        "{{\"host\":\"127.0.0.1\",\"port\":12345,\"user\":\"ku_test_{}\",\"password\":\"{}\",\"database\":\"ku_db_{}\"}}",
        "a".repeat(16), "b".repeat(64), "c".repeat(16)
    );
    assert!(private_config_password(&valid).is_some());
    for invalid in [
        valid.replace("127.0.0.1", "localhost"),
        valid.replace("127.0.0.1", "192.0.2.1"),
        valid.replace("12345", "330"),
        valid.replace("12345", "65536"),
        valid.replace("12345", "012345"),
        valid.replace("12345", "1.2345e4"),
        valid.replace("ku_test_", "business_"),
        valid.replace("ku_db_", "production_"),
        valid.replace(&"b".repeat(64), "short"),
        valid.replace("}", ",\"ssl\":true}"),
        valid.replace("}", ",\"port\":12345}"),
        valid.replace("\"port\":12345,", ""),
        valid.replace("\"user\"", "\"u\\u0073er\""),
        format!("{valid} trailing"),
        format!("{}{}", " ".repeat(CONFIG_LIMIT as usize), valid),
    ] {
        assert!(
            private_config_password(&invalid).is_none(),
            "invalid fixture accepted"
        );
    }
}

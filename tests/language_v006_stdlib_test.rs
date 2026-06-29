use std::{
    env, fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ku::cli::{check_source, run_source};

fn unique_temp_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-v006-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should work")
            .as_nanos()
    ))
}

fn check_err(source: &str) -> String {
    check_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

fn run_err(source: &str) -> String {
    run_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

#[test]
fn std_string_array_json_and_time_check_and_run() {
    let source = r#"
fn main() {
    text = string.trim("  Ku Lang  ")
    print(string.upper(text))
    print(string.contains(text, "Lang"))
    print(string.starts_with(text, "Ku"))
    print(string.ends_with(text, "Lang"))
    print(string.replace(text, "Lang", "0.0.8"))

    nums:[int] = [1, 2]
    nums = array.push(nums, 3)
    nums = array.concat(nums, [4])
    print(array.len(nums))
    print(array.first(nums))
    print(array.last(nums))
    print(array.is_empty(nums))

    data = { name: "Ku", version: 6, ok: true }
    json_text = json.stringify(data)
    parsed = json.parse(json_text)
    print(json.stringify(parsed))

    now = time.now()
    unix:int = time.unix(now)
    millis:int = time.millis()
    print(unix <= millis)
}
"#;

    check_source("inline.ku", source).expect("stdlib program should check");
    run_source("inline.ku", source).expect("stdlib program should run");
}

#[test]
fn std_string_and_array_functions_work_as_methods() {
    let source = r#"
fn main() {
    text = "  Ku  "
    if (text.trim().upper() != "KU") {
        panic("bad string method")
    }
    values:[int] = [1, 2]
    values = values.push(3)
    if (values.len() != 3) {
        panic("bad array len method")
    }
    if (values.first() != 1) {
        panic("bad array first method")
    }
}
"#;

    check_source("inline.ku", source).expect("stdlib methods should check");
    run_source("inline.ku", source).expect("stdlib methods should run");
}

#[test]
fn std_json_try_parse_integrates_with_question_and_try_catch() {
    let source = r#"
fn parse_value(): str! {
    value = json.try_parse("{bad}")?
    return ok(json.stringify(value))
}

fn main() {
    message = "none"
    try {
        message = parse_value()?
    } catch (err) {
        message = "json failed"
    }
    print(message)
}
"#;

    check_source("inline.ku", source).expect("json.try_parse program should check");
    run_source("inline.ku", source).expect("json.try_parse program should run");
}

#[test]
fn stdlib_recoverable_errors_are_structured_objects() {
    let source = r#"
import "std.fs"

fn parse_bad(): null! {
    json.try_parse("{bad}")?
    return ok(null)
}

fn read_bad(): null! {
    fs.try_read("definitely-missing-ku-file.txt")?
    return ok(null)
}

fn main() {
    try {
        parse_bad()?
    } catch (err) {
        if (err.domain != "json") {
            panic("bad json domain")
        }
        if (err.code != "parse_error") {
            panic("bad json code")
        }
    }

    try {
        read_bad()?
    } catch (err) {
        if (err.domain != "fs") {
            panic("bad fs domain")
        }
        if (err.code != "read_failed") {
            panic("bad fs code")
        }
    }
}
"#;

    check_source("inline.ku", source).expect("structured errors should check");
    run_source("inline.ku", source).expect("structured errors should run");
}

#[test]
fn std_fs_write_and_try_write_round_trip() {
    let dir = unique_temp_path("fs-write");
    fs::create_dir_all(&dir).expect("create temp dir");
    let target = dir.join("out.txt");
    let source = format!(
        r#"
import "std.fs"

fn save(path:str): null! {{
    return fs.try_write(path, "hello ku")
}}

fn main(): null! {{
    save("{}")?
    print(fs.read("{}"))
    return ok(null)
}}
"#,
        target.display().to_string().replace('\\', "\\\\"),
        target.display().to_string().replace('\\', "\\\\"),
    );

    check_source("inline.ku", &source).expect("fs write should check");
    run_source("inline.ku", &source).expect("fs write should run");
    assert_eq!(
        fs::read_to_string(&target).expect("read output"),
        "hello ku"
    );
    fs::remove_dir_all(dir).expect("remove temp dir");
}

#[test]
fn std_http_response_api_uses_response_object_without_external_network() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local http");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 512];
            loop {
                let read = stream.read(&mut buffer).expect("read request");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&bytes);
            let body = if request.starts_with("POST ") {
                "posted"
            } else {
                "hello"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Ku: yes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().expect("flush response");
            let _ = stream.shutdown(Shutdown::Write);
            thread::sleep(Duration::from_millis(20));
        }
    });
    let source = format!(
        r#"
import "std.http"

fn main(): null! {{
    res = http.get("http://{}")?
    if (res.status != 200) {{
        panic("bad status")
    }}
    if (res.body != "hello") {{
        panic("bad body")
    }}
    posted = http.request({{ method: "POST", url: "http://{}", body: "payload", timeout_ms: 1000 }})?
    if (posted.body != "posted") {{
        panic("bad post")
    }}
    return ok(null)
}}
"#,
        addr, addr
    );

    check_source("inline.ku", &source).expect("http response api should check");
    run_source("inline.ku", &source).expect("http response api should run");
    server.join().expect("server should finish");
}

#[test]
fn std_http_helpers_and_default_service_are_checked_and_runtime_safe() {
    let source = r#"
import "std.http"

fn main() {
    text = http.text("hello")
    if (text.status != 200) {
        panic("bad text status")
    }
    if (text.body != "hello") {
        panic("bad text body")
    }
    created = http.text(http.status.created, "created")
    if (created.status != 201) {
        panic("bad explicit text status")
    }
    json_res = http.json({ ok: true, count: 2 })
    if (json_res.headers["content-type"] != "application/json") {
        panic("bad json content type")
    }
    created_json = http.json(http.status.created, { id: 1 })
    if (created_json.status != 201) {
        panic("bad explicit json status")
    }
    if (http.statusText(http.status.notFound) != "Not Found") {
        panic("bad status text")
    }
    empty = http.empty()
    if (empty.status != 204 || empty.body != "") {
        panic("bad empty response")
    }
    redirect = http.redirect(http.status.temporaryRedirect, "/next")
    if (redirect.status != 307 || redirect.headers["location"] != "/next") {
        panic("bad redirect response")
    }
    code = http.code
    if (code.SUCCESS != 200) {
        panic("bad http code alias")
    }
    client = http.client()
    if (client.kind != "http.client") {
        panic("bad client")
    }
    service = http.service()
    if (service.kind != "http.service") {
        panic("bad service")
    }
    server = http.server()
    if (server.max_active_requests <= 0) {
        panic("bad server limits")
    }
    if (server.max_pending_requests <= 0) {
        panic("bad pending limit")
    }
    tuned = http.server({
        max_body_bytes: 4,
        read_header_timeout_ms: 500,
        max_connections: 2,
        max_active_requests: 1,
        max_pending_requests: 1
    })
    if (tuned.max_body_bytes != 4) {
        panic("bad configured server")
    }
    if (tuned.max_connections != 2 || tuned.max_active_requests != 1 || tuned.max_pending_requests != 1) {
        panic("bad configured backpressure")
    }
    client2 = http.client({ timeout_ms: 1000 })
    if (client2.timeout_ms != 1000) {
        panic("bad configured client")
    }
}
"#;

    check_source("inline.ku", source).expect("http helpers should check");
    run_source("inline.ku", source).expect("http helpers should run");
}

#[test]
fn std_http_config_limits_must_be_positive() {
    for source in [
        r#"
import "std.http"

fn main() {
    http.server({ max_header_bytes: 0 })
}
"#,
        r#"
import "std.http"

fn main() {
    http.client({ timeout_ms: -1 })
}
"#,
    ] {
        let err = run_err(source);
        assert!(
            err.contains("must be a positive int"),
            "unexpected error: {err}"
        );
    }
}

#[test]
fn std_http_service_route_methods_register_routes() {
    let source = r#"
import "std.http"

fn main() {
    app = http.service
    app.get("/index", (req, res) => {
        return http.text("ok")
    })
    app.post("/pets", (req, res) => {
        return http.json({ ok: true })
    })
    app.get("/user/{id}", (req, res) => {
        return http.text(req.params.id)
    })
    if (array.len(app.routes) != 3) {
        panic("routes were not registered")
    }
    if (app.routes[0].method != "GET") {
        panic("bad route method")
    }
    if (app.routes[0].path != "/index") {
        panic("bad route path")
    }
    if (app.routes[2].param_names[0] != "id") {
        panic("bad route param")
    }
}
"#;

    check_source("inline.ku", source).expect("http service routes should check");
    run_source("inline.ku", source).expect("http service routes should run");
}

#[test]
fn std_http_service_bind_returns_listener_result() {
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.service
    listener = app.bind(":0")?
    if (listener.kind != "http.listener") {
        panic("bad listener")
    }
    listener.close()?
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("http service bind should check");
    run_source("inline.ku", source).expect("http service bind should run");
}

#[test]
fn std_http_listener_close_consumes_listener() {
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.service
    listener = app.bind(":0")?
    listener.close()?
    try {
        listener.close()?
        panic("second close should fail")
    } catch (err) {
        if (err.code != "close_failed") {
            panic("bad close error")
        }
    }
    try {
        listener.run()?
        panic("run after close should fail")
    } catch (err) {
        if (err.code != "run_failed") {
            panic("bad run error")
        }
    }
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("http listener close should check");
    run_source("inline.ku", source).expect("http listener close should run");
}

#[test]
fn std_http_service_bind_compiles_routes() {
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.service
    app.get("/user/{id}", (req, res) => http.text("ok"))
    listener = app.bind(":0")?
    if (listener.kind != "http.listener") {
        panic("bad listener")
    }
    if (listener.compiled_router["GET"]["/user/{}"].path != "/user/{id}") {
        panic("bad compiled route")
    }
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("http service bind should check");
    run_source("inline.ku", source).expect("http service bind should run");
}

#[test]
fn std_http_service_rejects_invalid_or_duplicate_routes_before_bind() {
    let duplicate = r#"
import "std.http"

fn main(): null! {
    app = http.service
    app.get("/user/{id}", (req, res) => http.text("one"))
    app.get("/user/{name}", (req, res) => http.text("two"))
    app.bind(":0")?
    return ok(null)
}
"#;
    let err = run_err(duplicate);
    assert!(
        err.contains("duplicate http route"),
        "unexpected error: {err}"
    );

    let express_style = r#"
import "std.http"

fn main() {
    app = http.service
    app.get("/user/:id", (req, res) => http.text("bad"))
}
"#;
    let err = run_err(express_style);
    assert!(
        err.contains("http route params use '{name}'"),
        "unexpected error: {err}"
    );

    let route_config = r#"
import "std.http"

fn main() {
    app = http.service
    app.get("/user/{id}", { auth: "none" }, (req, res) => http.text("bad"))
}
"#;
    let err = check_err(route_config);
    assert!(
        err.contains("expects 2 arguments but got 3"),
        "unexpected error: {err}"
    );

    let bind_config = r#"
import "std.http"

fn main() {
    app = http.service
    app.bind(":0", { max_body_bytes: 4 })
}
"#;
    let err = check_err(bind_config);
    assert!(
        err.contains("expects 1 argument but got 2"),
        "unexpected error: {err}"
    );
}

#[test]
fn std_http_handler_signature_and_capture_rules_are_checked() {
    let valid = r#"
import "std.http"

fn main() {
    app = http.service
    app.get("/user/{id}", (req, res) => {
        if (req.method != "GET") {
            panic("bad method")
        }
        return http.text(req.params.id)
    })
}
"#;
    check_source("inline.ku", valid).expect("http handler request fields should check");

    let wrong_arity = r#"
import "std.http"

fn main() {
    app = http.service
    app.get("/", (req) => http.text("bad"))
}
"#;
    let err = check_err(wrong_arity);
    assert!(
        err.contains("handler expects 2 parameters"),
        "unexpected error: {err}"
    );

    let bad_return = r#"
import "std.http"

fn main() {
    app = http.service
    app.get("/", (req, res) => "bad")
}
"#;
    let err = check_err(bad_return);
    assert!(
        err.contains("expected object but got str"),
        "unexpected error: {err}"
    );

    let captured_assign = r#"
import "std.http"

fn main() {
    count = 0
    app = http.service
    app.get("/", (req, res) => {
        count = count + 1
        return http.text("bad")
    })
}
"#;
    let err = check_err(captured_assign);
    assert!(
        err.contains("cannot modify captured variable 'count'"),
        "unexpected error: {err}"
    );

    let captured_field_assign = r#"
import "std.http"

fn main() {
    state = { n: 0 }
    app = http.service
    app.get("/", (req, res) => {
        state.n = 1
        return http.text("bad")
    })
}
"#;
    let err = check_err(captured_field_assign);
    assert!(
        err.contains("cannot modify captured variable 'state'"),
        "unexpected error: {err}"
    );
}

#[test]
fn std_config_reads_env_and_flat_yaml_relative_to_source_file() {
    let dir = unique_temp_path("config");
    fs::create_dir_all(&dir).expect("create temp config dir");
    fs::write(
        dir.join(".env"),
        "APP_NAME=ku\nQUOTED=\"hello world\"\nESCAPED=\"a\\nb\"\n",
    )
    .expect("write .env");
    fs::write(dir.join("extra.env"), "TOKEN='abc'\n").expect("write extra env");
    fs::write(
        dir.join("app.yaml"),
        "port: 8080\ndebug: true\nname: ku\npi: 3.5\n",
    )
    .expect("write yaml");
    let source_path = dir.join("main.ku");
    let source_file = source_path.to_string_lossy().into_owned();
    let source = r#"
import "std.config"

fn main(): null! {
    env = config.env()
    if (env.APP_NAME != "ku") {
        panic("bad env")
    }
    if (env.QUOTED != "hello world") {
        panic("bad quoted env")
    }
    extra = config.env_file("extra.env")
    if (extra.TOKEN != "abc") {
        panic("bad env_file")
    }
    cfg = config.yaml("app.yaml")?
    if (cfg.port != 8080) {
        panic("bad yaml int")
    }
    if (cfg.debug != true) {
        panic("bad yaml bool")
    }
    if (cfg.name != "ku") {
        panic("bad yaml string")
    }
    return ok(null)
}
"#;

    check_source(&source_file, source).expect("std.config program should check");
    run_source(&source_file, source).expect("std.config program should run");
    fs::remove_dir_all(&dir).expect("remove temp config dir");
}

#[test]
fn std_config_import_gate_and_error_paths_are_checked() {
    let missing_import = r#"
fn main() {
    env = config.env()
}
"#;
    let err = check_err(missing_import);
    assert!(
        err.contains("std module 'config' must be imported"),
        "unexpected error: {err}"
    );

    let dir = unique_temp_path("config-errors");
    fs::create_dir_all(&dir).expect("create temp config dir");
    fs::write(dir.join("bad.yaml"), "nested:\n  value: 1\n").expect("write bad yaml");
    let source_path = dir.join("main.ku");
    let source_file = source_path.to_string_lossy().into_owned();
    let source = r#"
import "std.config"

fn main() {
    env = config.env()
    print("empty env ok")
    try {
        config.yaml("bad.yaml")?
        panic("bad yaml should fail")
    } catch (err) {
        if (err.domain != "config") {
            panic("bad yaml domain")
        }
    }
    config.env_file("missing.env")
}
"#;
    let err = run_source(&source_file, source)
        .expect_err("missing env_file should be unrecoverable")
        .to_string();
    assert!(err.contains("cannot be read"), "unexpected error: {err}");
    fs::remove_dir_all(&dir).expect("remove temp config dir");
}

#[test]
fn stdlib_type_errors_are_checked_before_run() {
    for source in [
        r#"fn main() { print(string.len(123)) }"#,
        r#"fn main() { print(string.contains("Ku", 1)) }"#,
        r#"fn main() { xs:[int] = [1]; xs = array.push(xs, "bad") }"#,
        r#"fn main() { xs:[int] = [1]; xs = array.concat(xs, ["bad"]) }"#,
        r#"fn main() { print(json.parse(123)) }"#,
        r#"fn main() { print(time.now(1)) }"#,
        r#"fn main() { print(time.unix(1)) }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("error:"), "unexpected error: {err}");
    }
}

#[test]
fn std_time_documented_api_check_and_run() {
    let source = r#"
import { time } from "std"

fn main(): null! {
    t = time.from_millis(1782210600123)
    if (time.unix(t) != 1782210600) {
        panic("bad unix")
    }
    if (time.millis(t) != 1782210600123) {
        panic("bad millis")
    }

    d = time.date(2026, 6, 23)?
    if (time.weekday(d) != 2) {
        panic("bad weekday")
    }
    if (time.days_in_month(2026, 2)? != 28) {
        panic("bad days")
    }
    if (!time.is_leap(2028)) {
        panic("bad leap")
    }

    duration = time.duration(5, "s")?
    if (time.millis(duration) != 5000) {
        panic("bad duration")
    }
    later = time.add(t, duration)
    if (time.diff(later, t).millis != 5000) {
        panic("bad diff")
    }
    if (time.compare(later, t) != 1) {
        panic("bad compare")
    }

    text = time.format(t, "yyyy-MM-dd HH:mm:ss", "utc")?
    parsed = time.parse(text, "yyyy-MM-dd HH:mm:ss", "utc")?
    if (time.millis(parsed) != 1782210600000) {
        panic("bad parse")
    }
    parts = time.parts(t, "+08:00")?
    if (parts.year != 2026 || parts.month != 6 || parts.day != 23) {
        panic("bad parts")
    }
    time.sleep(time.duration(0)?)?
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("std.time documented api should check");
    run_source("inline.ku", source).expect("std.time documented api should run");

    let err = run_source(
        "inline.ku",
        r#"fn main(): null! { time.date(2026, 13, 40)? return ok(null) }"#,
    )
    .expect_err("bad date should propagate Result error");
    assert!(
        err.to_string().contains("invalid_date"),
        "unexpected error: {err}"
    );
}

#[test]
fn std_time_rejects_out_of_range_millis_without_falling_back_to_now() {
    let source = r#"
import { time } from "std"

fn main(): null! {
    value = time.from_millis(9223372036854775807)
    print(time.format(value))
    return ok(null)
}
"#;
    let err = run_source("inline.ku", source)
        .expect_err("out-of-range millis should be rejected")
        .to_string();
    assert!(
        err.contains("outside supported range"),
        "error should explain the range problem: {err}"
    );
}

#[test]
fn stdlib_modules_can_be_shadowed_by_local_values() {
    let source = r#"
fn main() {
    string = { len: "local string" }
    array = { len: "local array" }
    json = { parse: "local json" }
    time = { now: "local time" }

    print(string.len)
    print(array.len)
    print(json.parse)
    print(time.now)
}
"#;

    check_source("inline.ku", source).expect("shadowing should check");
    run_source("inline.ku", source).expect("shadowing should run");
}

#[test]
fn json_runtime_errors_are_clear_and_bounded() {
    let parse_error = run_err(r#"fn main() { print(json.parse("{bad}")) }"#);
    assert!(
        parse_error.contains("expected") || parse_error.contains("json"),
        "unexpected error: {parse_error}"
    );

    let stringify_error = run_err(
        r#"
fn main() {
    value = ok(1)
    print(json.stringify(value))
}
"#,
    );
    assert!(
        stringify_error.contains("json.stringify does not support result"),
        "unexpected error: {stringify_error}"
    );

    for bad_json in ["1.", "1e", "01"] {
        let source = format!(r#"fn main() {{ print(json.parse("{bad_json}")) }}"#);
        let err = run_err(&source);
        assert!(
            err.contains("digit") || err.contains("leading zero"),
            "unexpected error for {bad_json}: {err}"
        );
    }
}

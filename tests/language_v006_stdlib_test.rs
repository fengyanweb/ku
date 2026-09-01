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
fn main(): null! {
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
    json_text = json.stringify(data)?
    parsed = json.parse(json_text)?
    print(json.stringify(parsed)?)

    now:int = time.now()
    instant = time.instant()
    unix:int = time.unix(instant)
    millis:int = time.millis()
    print(now > 0)
    print(unix > 0)
    print(millis > 0)
    return ok(null)
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
    unicode = "A界😀"
    scalars:[str] = unicode.chars()
    if (scalars.len() != 3 || scalars[0] != "A" || scalars[1] != "界" || scalars[2] != "😀") {
        panic("bad Unicode scalar split")
    }
    global_scalars:[str] = string.chars(unicode)
    if (global_scalars.len() != 3 || global_scalars[1] != "界") {
        panic("bad global string.chars")
    }
    if (unicode != "A界😀" || "".chars().len() != 0) {
        panic("string chars must borrow its receiver")
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
fn std_string_byte_len_counts_utf8_and_borrows_owned_values() {
    let source = concat!(
        r#"
fn VerifyByteLength(text: str, bytes: int, scalars: int) {
    method_bytes:int = text.byte_len()
    module_bytes:int = string.byte_len(text)
    if (method_bytes != bytes || module_bytes != bytes) {
        panic("byte_len must count UTF-8 bytes")
    }
    if (text.len() != scalars || string.len(text) != scalars || len(text) != scalars) {
        panic("len must still count Unicode scalars")
    }
    if (text.byte_len() != bytes || string.byte_len(text) != bytes) {
        panic("byte_len must borrow its receiver")
    }
}

fn main() {
    VerifyByteLength("", 0, 0)
    VerifyByteLength("ASCII", 5, 5)
    VerifyByteLength("界", 3, 1)
    VerifyByteLength("😀", 4, 1)
"#,
        "    VerifyByteLength(\"e\u{301}\", 3, 2)\n",
        // The outer Rust literal supplies a real NUL, not a Ku escape sequence.
        "    VerifyByteLength(\"A\0界😀\", 9, 4)\n",
        r#"
    owned = "A" + "界😀"
    if (owned.byte_len() != 8 || string.byte_len(owned) != 8 || owned.len() != 3) {
        panic("owned string byte length is wrong")
    }
    copy = owned.clone()
    owned += "!"
    if (copy.byte_len() != 8 || string.byte_len(copy) != 8 || copy != "A界😀") {
        panic("byte_len consumed or changed an owned clone")
    }
    moved = copy
    VerifyByteLength(moved, 8, 3)
    VerifyByteLength(owned, 9, 4)
}
"#,
    );

    check_source("string-byte-len.ku", source)
        .expect("byte_len signatures and borrowing should check");
    run_source("string-byte-len.ku", source)
        .expect("byte_len UTF-8 and ownership cases should run");
}

#[test]
fn std_string_byte_len_rejects_wrong_types_and_arity() {
    for (source, expected) in [
        (
            r#"fn main() { print(string.byte_len(123)) }"#,
            "type error: expected str but got int",
        ),
        (
            r#"fn main() { value = 123; print(value.byte_len()) }"#,
            "type error: int has no fields",
        ),
        (
            r#"fn main() { value:str = "Ku".byte_len() }"#,
            "type error: expected str but got int",
        ),
        (
            r#"fn main() { print(string.byte_len()) }"#,
            "function 'string.byte_len' expects 1 arguments but got 0",
        ),
        (
            r#"fn main() { print(string.byte_len("Ku", "extra")) }"#,
            "function 'string.byte_len' expects 1 arguments but got 2",
        ),
        (
            r#"fn main() { print("Ku".byte_len(1)) }"#,
            "function 'string.byte_len' expects 1 arguments but got 2",
        ),
    ] {
        let error = check_err(source);
        assert!(
            error.contains(expected),
            "unexpected byte_len diagnostic for {source}: {error}"
        );
    }
}

#[test]
fn std_json_try_parse_integrates_with_question_and_try_catch() {
    let source = r#"
fn parse_value(): str! {
    value = json.try_parse("{bad}")?
    return json.stringify(value)
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
fn std_json_rejects_non_finite_numbers_in_parse_and_stringify() {
    let source = r#"
fn parse_overflow(): null! {
    json.try_parse("1e400")?
    return ok(null)
}

fn main(): null! {
    caught = false
    try {
        parse_overflow()?
    } catch (err) {
        caught = true
        if (err.domain != "json" || err.code != "parse_error") {
            panic("bad non-finite json parse error")
        }
    }
    if (!caught) {
        panic("non-finite json number should fail")
    }
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("non-finite json Result flow should check");
    run_source("inline.ku", source).expect("non-finite json Result flow should run");

    let parse_error =
        run_err(r#"fn main(): null! { print(json.parse("1e400")?) return ok(null) }"#);
    assert!(
        parse_error.contains("json number must be finite"),
        "unexpected error: {parse_error}"
    );

    let stringify_error = run_err(
        r#"
fn main(): null! {
    value = 1.0
    i = 0
    while (i < 1024) {
        value = value * 2.0
        i = i + 1
    }
    print(json.stringify(value)?)
    return ok(null)
}
"#,
    );
    assert!(
        stringify_error.contains("json.stringify does not support non-finite float"),
        "unexpected error: {stringify_error}"
    );
}

#[test]
fn std_json_dynamic_index_type_errors_are_structured_results() {
    let source = r#"
import json from "std.json"

fn main(): null! {
    scalar = json.parse("1")?
    object_error = false
    try {
        value = scalar["age"]?
        print(value)
    } catch (err) {
        object_error = err.domain == "object" && err.code == "type_unsupported" && err.message == "expected object value"
    }
    if (!object_error) {
        panic("dynamic object type error was not structured")
    }

    object = json.parse("{}")?
    array_error = false
    try {
        value = object[0]?
        print(value)
    } catch (err) {
        array_error = err.domain == "array" && err.code == "not_an_array" && err.message == "expected array value"
    }
    if (!array_error) {
        panic("dynamic array type error was not structured")
    }
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("dynamic index Result errors should check");
    run_source("inline.ku", source).expect("dynamic index Result errors should run");
}

#[test]
fn std_json_accepts_unicode_surrogate_pairs_and_rejects_isolated_units() {
    let source = r#"
import json from "std.json"

fn Decode(text: str): null! {
    json.try_parse(text)?
    return ok(null)
}

fn main(): null! {
    value = json.parse("\"\\uD83D\\uDE00\"")?
    decoded = value.as_str()?
    if (decoded != "😀") {
        panic("surrogate pair decoded incorrectly")
    }

    rejected_low = false
    try {
        Decode("\"\\uDE00\"")?
    } catch (err) {
        rejected_low = err.domain == "json" && err.code == "parse_error"
    }
    rejected_high = false
    try {
        Decode("\"\\uD83Dx\"")?
    } catch (err) {
        rejected_high = err.domain == "json" && err.code == "parse_error"
    }
    if (!rejected_low || !rejected_high) {
        panic("isolated surrogate should fail")
    }
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("unicode surrogate Result flow should check");
    run_source("inline.ku", source).expect("unicode surrogate Result flow should run");
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
    print(fs.read("{}")?)
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
fn std_fs_exists_and_read_dir_are_sorted_and_structured() {
    let dir = unique_temp_path("fs-read-dir");
    fs::create_dir_all(&dir).expect("create temp dir");
    let first = dir.join("a.txt");
    let second = dir.join("b.txt");
    let missing = dir.join("missing");
    fs::write(&second, "second").expect("write second entry");
    fs::write(&first, "first").expect("write first entry");
    let ku_path = |path: &PathBuf| path.display().to_string().replace('\\', "\\\\");
    let source = format!(
        r#"
import "std.fs"

fn list(path:str): [str]! {{
    return fs.read_dir(path)
}}

fn list_missing(path:str): null! {{
    fs.read_dir(path)?
    return ok(null)
}}

fn main(): null! {{
    if (!fs.exists("{}")) {{
        panic("directory should exist")
    }}
    if (!fs.exists("{}")) {{
        panic("file should exist")
    }}
    if (fs.exists("{}")) {{
        panic("missing path should not exist")
    }}

    entries = list("{}")?
    if (entries.len() != 2) {{
        panic("bad directory entry count")
    }}
    if (entries[0] != "{}" || entries[1] != "{}") {{
        panic("directory entries should be stable and sorted")
    }}

    caught = false
    try {{
        list_missing("{}")?
    }} catch (err) {{
        caught = true
        if (err.domain != "fs" || err.code != "read_dir_failed") {{
            panic("bad read_dir error")
        }}
    }}
    if (!caught) {{
        panic("missing directory should fail")
    }}
    return ok(null)
}}
"#,
        ku_path(&dir),
        ku_path(&first),
        ku_path(&missing),
        ku_path(&dir),
        ku_path(&first),
        ku_path(&second),
        ku_path(&missing),
    );

    check_source("inline.ku", &source).expect("fs directory api should check");
    run_source("inline.ku", &source).expect("fs directory api should run");
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
fn std_http_client_rejects_header_and_url_injection_before_network_io() {
    for (label, source, expected) in [
        (
            "invalid header name",
            r#"import "std.http"
fn main() {
    headers = {}
    headers["Bad Name"] = "x"
    http.request({ url: "http://127.0.0.1/", headers: headers })
}"#,
            "not a valid HTTP token",
        ),
        (
            "header value injection",
            r#"import "std.http"
fn main() {
    headers = {}
    headers["x-test"] = "ok\r\ninjected: yes"
    http.request({ url: "http://127.0.0.1/", headers: headers })
}"#,
            "contains control characters",
        ),
        (
            "transport-managed framing header",
            r#"import "std.http"
fn main() {
    headers = {}
    headers["Content-Length"] = "0"
    http.request({ url: "http://127.0.0.1/", headers: headers })
}"#,
            "managed by the HTTP transport",
        ),
        (
            "case-insensitive duplicate header",
            r#"import "std.http"
fn main() {
    headers = {}
    headers["X-Test"] = "one"
    headers["x-test"] = "two"
    http.request({ url: "http://127.0.0.1/", headers: headers })
}"#,
            "duplicate http.request header",
        ),
        (
            "URL CRLF injection",
            r#"import "std.http"
fn main(): null! { http.request({ url: "http://127.0.0.1/\r\ninjected" })? return ok(null) }"#,
            "must not contain whitespace or control characters",
        ),
    ] {
        let err = run_err(source);
        assert!(err.contains(expected), "unexpected {label} error: {err}");
    }
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
    html = http.html("<h1>Ku</h1>")
    if (html.headers["content-type"] != "text/html; charset=utf-8") {
        panic("bad html content type")
    }
    json_res = http.json({ ok: true, count: 2 })
    if (json_res.headers["content-type"] != "application/json; charset=utf-8") {
        panic("bad json content type")
    }
    created_json = http.json(http.status.created, { id: 1 })
    if (created_json.status != 201) {
        panic("bad explicit json status")
    }
    if (http.statusText(http.status.notFound) != "Not Found") {
        panic("bad status text")
    }
    if (http.statusText(418) != "Unknown") {
        panic("unknown status must not be described as OK")
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
fn std_http_service_requires_constructor_call() {
    for source in [
        r#"
import "std.http"

fn main() {
    app = http.service
}
"#,
        r#"
import "std.http"

fn main() {
    app = http.server
}
"#,
        r#"
import "std.http"

fn main() {
    http.service.get("/", fn() {
        return http.text("bad")
    })
}
"#,
        r#"
import "std.http"

fn main() {
    app = http.service.kind
}
"#,
    ] {
        let err = check_err(source);
        assert!(
            err.contains("call it as 'http.service()'")
                || err.contains("call it as 'http.server()'"),
            "unexpected error: {err}"
        );
    }
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
    app = http.service()
    registered = app.get("/index", fn() {
        return http.text("ok")
    })
    if (registered != null) {
        panic("route registration must not alias the service")
    }
    app.post("/pets", fn() {
        return http.json({ ok: true })
    })
    app.get("/user/{id}", fn(req) {
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
fn std_http_keeps_snake_case_config_and_del_only_route_api() {
    for (source, field) in [
        (
            r#"
import "std.http"
fn main() {
    http.server({ maxActiveRequests: 1 })
}
"#,
            "maxActiveRequests",
        ),
        (
            r#"
import "std.http"
fn main() {
    http.client({ maxIdleConnections: 1 })
}
"#,
            "maxIdleConnections",
        ),
        (
            r#"
import "std.http"
fn main() {
    http.client({ max_idle_connection: 1 })
}
"#,
            "max_idle_connection",
        ),
        (
            r#"
import "std.http"
fn main(): null! {
    response = http.request({ url: "http://127.0.0.1", maxBodyBytes: 1 })?
    return ok(null)
}
"#,
            "maxBodyBytes",
        ),
        (
            r#"
import "std.http"
fn main(): null! {
    response = http.request({ url: "http://127.0.0.1", timeout_mss: 1 })?
    return ok(null)
}
"#,
            "timeout_mss",
        ),
    ] {
        let config_error = check_err(source);
        assert!(
            config_error.contains(&format!("unknown http config field '{field}'")),
            "unexpected HTTP config error for {field}: {config_error}"
        );
    }

    let route_error = check_err(
        r#"
import "std.http"
fn main() {
    app = http.service()
    app.delete("/user/{id}", fn(req) {
        return http.text(req.params.id)
    })
}
"#,
    );
    assert!(
        route_error.contains("object has no field 'delete'"),
        "unexpected delete alias error: {route_error}"
    );
}

#[test]
fn std_http_handler_param_inferred_from_route_signature() {
    // C: an HTTP route handler `fn(req) { ... }` needs no annotation on `req`;
    // the route API supplies the handler signature, so `req` is typed as the
    // request and `req.params` is available. Checker and interpreter agree.
    let source = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/user/{id}", fn(req) {
        return http.text(req.params.id)
    })
    if (app.routes[0].param_names[0] != "id") {
        panic("handler with inferred req did not register")
    }
}
"#;
    check_source("inline.ku", source).expect("inferred http handler parameter should check");
    run_source("inline.ku", source).expect("inferred http handler should run");
}

#[test]
fn std_http_service_bind_returns_listener_result() {
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.service()
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
    app = http.service()
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
fn std_http_listen_consumes_service_even_when_error_is_caught() {
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.service()
    try {
        app.listen("not-an-address")?
    } catch (err) {
        print(err.code)
    }
    app.get("/after-listen", fn() { return http.text("unsafe") })
    return ok(null)
}
"#;
    let err = check_err(source);
    assert!(
        err.contains("use of moved value 'app'"),
        "unexpected error: {err}"
    );

    let clone_err = check_err(
        r#"import "std.http"
fn main() {
    app = http.service()
    copy = app.clone()
}"#,
    );
    assert!(
        clone_err.contains("http service values cannot be cloned"),
        "unexpected clone error: {clone_err}"
    );
}

#[test]
fn std_http_service_bind_compiles_routes() {
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.service()
    app.get("/user/{id}", fn() {
        return http.text("ok")
    })
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
    app = http.service()
    app.get("/user/{id}", fn() {
        return http.text("one")
    })
    app.get("/user/{name}", fn() {
        return http.text("two")
    })
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
    app = http.service()
    app.get("/user/:id", fn() {
        return http.text("bad")
    })
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
    app = http.service()
    app.get("/user/{id}", { auth: "none" }, fn() {
        return http.text("bad")
    })
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
    app = http.service()
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
    app = http.service()
    app.get("/user/{id}", fn(req) {
        if (req.method != "GET") {
            panic("bad method")
        }
        return http.text(req.params.id)
    })
}
"#;
    check_source("inline.ku", valid).expect("http handler request fields should check");

    let valid_no_req = r#"
import "std.http"

fn health() {
    return http.text("ok")
}

fn main() {
    app = http.service()
    app.get("/", health)
    app.get("/inline", fn() {
        return http.text("inline")
    })
    app.get("/adapter", fn(_req) {
        return http.text("adapter")
    })
}
"#;
    check_source("inline.ku", valid_no_req).expect("http handlers may omit req");

    let wrong_arity = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn(req, res) {
        return http.text("bad")
    })
}
"#;
    let err = check_err(wrong_arity);
    assert!(
        err.contains("accepts fn() or fn(req); fn(req, res) is not allowed"),
        "unexpected error: {err}"
    );

    let unused_req = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn(req) {
        return http.text("bad")
    })
}
"#;
    let err = check_err(unused_req);
    assert!(err.contains("write fn()"), "unexpected error: {err}");

    let bad_return = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn() {
        return "bad"
    })
}
"#;
    let err = check_err(bad_return);
    assert!(
        err.contains("HTTP handler must return HttpResponse or HttpResponse!, but got str"),
        "unexpected error: {err}"
    );

    let missing_return = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn() {
        value = 1
    })
}
"#;
    let err = check_err(missing_return);
    assert!(
        err.contains("HTTP handler must return HttpResponse or HttpResponse!, but got null"),
        "unexpected error: {err}"
    );

    let incomplete_return = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn(req) {
        if (req.path == "/") {
            return http.text("ok")
        }
    })
}
"#;
    let err = check_err(incomplete_return);
    assert!(
        err.contains("HTTP handler must return HttpResponse or HttpResponse!, but got null"),
        "unexpected error: {err}"
    );

    let captured_assign = r#"
import "std.http"

fn main() {
    count = 0
    app = http.service()
    app.get("/", fn() {
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
    app = http.service()
    app.get("/", fn() {
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

    let side_effect_response = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn(req) {
        res.write("bad")
        return http.text(req.path)
    })
}
"#;
    let err = check_err(side_effect_response);
    assert!(
        err.contains("side-effect response API 'res.write' is not allowed"),
        "unexpected error: {err}"
    );

    let result_return = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn() {
        return ok(http.text("ok"))
    })
}
"#;
    check_source("inline.ku", result_return).expect("HttpResponse! handlers should check");
}

#[test]
fn std_http_handlers_cannot_control_captured_services_or_listeners() {
    for (constructor, method, call) in [
        (
            "service",
            "get",
            r#"app.get("/late", fn() { return http.text("late") })"#,
        ),
        (
            "server",
            "post",
            r#"app.post("/late", fn() { return http.text("late") })"#,
        ),
        (
            "service",
            "put",
            r#"app.put("/late", fn() { return http.text("late") })"#,
        ),
        (
            "service",
            "del",
            r#"app.del("/late", fn() { return http.text("late") })"#,
        ),
        ("service", "bind", r#"app.bind("127.0.0.1:0")"#),
        ("service", "listen", r#"app.listen("127.0.0.1:0")"#),
    ] {
        let source = format!(
            r#"
import "std.http"

fn main() {{
    app = http.{constructor}()
    app.get("/", fn() {{
        {call}
        return http.text("bad")
    }})
}}
"#
        );
        let err = check_err(&source);
        assert!(
            err.contains(&format!(
                "http handler cannot call '{method}' on captured http service rooted at 'app'"
            )),
            "unexpected captured service {method} error: {err}"
        );
        assert!(
            err.contains(
                "handlers cannot modify, start, run, or close captured services/listeners"
            ),
            "captured service diagnostic did not explain the rule: {err}"
        );
    }

    for method in ["run", "close"] {
        let source = format!(
            r#"
import "std.http"

fn main(): null! {{
    listener_app = http.service()
    listener = listener_app.bind("127.0.0.1:0")?
    app = http.service()
    app.get("/", fn() {{
        listener.{method}()
        return http.text("bad")
    }})
    return ok(null)
}}
"#
        );
        let err = check_err(&source);
        assert!(
            err.contains(&format!(
                "http handler cannot call '{method}' on captured http listener rooted at 'listener'"
            )),
            "unexpected captured listener {method} error: {err}"
        );
    }

    let arrow = r#"
import "std.http"

fn main() {
    app = http.service()
    handler = () => {
        app.post("/late", fn() { return http.text("late") })
        return http.text("bad")
    }
    app.get("/", handler)
}
"#;
    let err = check_err(arrow);
    assert!(
        err.contains("http handler cannot call 'post' on captured http service rooted at 'app'"),
        "arrow handler escaped captured service audit: {err}"
    );

    let reachable_named_handler = r#"
import "std.http"

fn main() {
    app = http.service()
    fn install_late_route() {
        app.put("/late", fn() { return http.text("late") })
    }
    fn handler() {
        install_late_route()
        return http.text("bad")
    }
    app.get("/", handler)
}
"#;
    let err = check_err(reachable_named_handler);
    assert!(
        err.contains("http handler cannot call 'put' on captured http service rooted at 'app'"),
        "reachable named handler escaped captured service audit: {err}"
    );

    let handler_local_control = r#"
import "std.http"

fn main() {
    app = http.service()
    app.get("/", fn() {
        local = http.service()
        local.del("/local", fn() { return http.text("local") })
        return http.text("bad")
    })
}
"#;
    let err = check_err(handler_local_control);
    assert!(
        err.contains("http handler cannot call 'del' on http service; HTTP control-plane calls are forbidden in handlers"),
        "handler-local control-plane call escaped the audit: {err}"
    );

    let ordinary_reads_are_allowed = r#"
import "std.http"

fn main() {
    enabled = true
    base = 40
    app = http.service()
    app.get("/", fn() {
        answer = base + 2
        if (enabled) {
            if (answer == 42) {
                return http.text("ok")
            }
        }
        return http.text("disabled")
    })
}
"#;
    check_source("inline.ku", ordinary_reads_are_allowed)
        .expect("ordinary handler locals and captured read-only values should stay valid");
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
fn dynamic_http_configs_reject_unknown_fields() {
    let dir = unique_temp_path("http-dynamic-config");
    fs::create_dir_all(&dir).expect("create temp http config dir");
    let source_path = dir.join("main.ku");
    let source_file = source_path.to_string_lossy().into_owned();
    for (name, yaml, expression, field) in [
        (
            "server",
            "maxActiveRequests: 1\n",
            "app = http.server(cfg)",
            "maxActiveRequests",
        ),
        (
            "client-camel",
            "maxIdleConnections: 1\n",
            "client = http.client(cfg)",
            "maxIdleConnections",
        ),
        (
            "client-typo",
            "max_idle_connection: 1\n",
            "client = http.client(cfg)",
            "max_idle_connection",
        ),
        (
            "request-camel",
            "url: http://127.0.0.1\nmaxBodyBytes: 1\n",
            "response = http.request(cfg)?",
            "maxBodyBytes",
        ),
        (
            "request-typo",
            "url: http://127.0.0.1\ntimeout_mss: 1\n",
            "response = http.request(cfg)?",
            "timeout_mss",
        ),
    ] {
        let yaml_name = format!("{name}.yaml");
        fs::write(dir.join(&yaml_name), yaml).expect("write dynamic http config");
        let source = format!(
            r#"
import http from "std.http"
import config from "std.config"

fn main(): null! {{
    cfg = config.yaml("{yaml_name}")?
    {expression}
    return ok(null)
}}
"#
        );

        check_source(&source_file, &source).expect("dynamic config shape is checked at runtime");
        let error = run_source(&source_file, &source)
            .expect_err("unknown dynamic http config field must be rejected")
            .to_string();
        assert!(
            error.contains(&format!("unknown http config field '{field}'")),
            "unexpected dynamic HTTP config error for {field}: {error}"
        );
    }
    fs::remove_dir_all(&dir).expect("remove temp http config dir");
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
        r#"fn main() { print(time.elapsed(1)) }"#,
        r#"fn main() { print(time.unix(1)) }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("error:"), "unexpected error: {err}");
    }

    for source in [
        r#"fn main() { print(time.now(1)) }"#,
        r#"fn main() {
    instant = time.instant()
    print(time.now(instant))
}"#,
    ] {
        let err = check_err(source);
        assert!(
            err.contains("function 'time.now' expects 0 arguments but got 1"),
            "unexpected legacy time.now overload error: {err}"
        );
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
fn std_http_config_limits_have_hard_maxima() {
    for (constructor, field, value, maximum) in [
        ("server", "read_header_timeout_ms", 300_001_i64, 300_000_i64),
        ("server", "max_header_bytes", 65_537, 65_536),
        ("server", "max_body_bytes", 16_777_217, 16_777_216),
        ("server", "max_connections", 4_097, 4_096),
        ("server", "max_active_requests", 1_025, 1_024),
        ("server", "max_pending_requests", 8_193, 8_192),
        ("client", "max_idle_connections", 1_025, 1_024),
    ] {
        let source = format!(
            "import \"std.http\"\nfn main() {{ http.{constructor}({{ {field}: {value} }}) }}"
        );
        let err = run_err(&source);
        assert!(
            err.contains(&format!("must be at most {maximum}")),
            "unexpected {constructor}.{field} error: {err}"
        );
    }

    let err = run_err(
        r#"import "std.http"
fn main() {
    app = http.service()
    app.max_body_bytes = 16777217
    app.bind(":0")
}"#,
    );
    assert!(
        err.contains("must be at most 16777216"),
        "post-construction assignment was not bounded: {err}"
    );
}

#[test]
fn std_http_route_registration_rejects_unreachable_or_unsafe_paths() {
    let segments = (0..65).map(|_| "s").collect::<Vec<_>>().join("/");
    let source = format!(
        "import \"std.http\"\nfn handler() {{ return http.text(\"ok\") }}\nfn main() {{\n app = http.service()\n app.get(\"/{segments}\", handler)\n}}"
    );
    let err = run_err(&source);
    assert!(
        err.contains("at most 64 segments"),
        "unexpected error: {err}"
    );

    for (label, path) in [
        ("NUL", "/bad\0tail"),
        ("control", "/bad\\ntail"),
        ("malformed percent escape", "/bad%2"),
    ] {
        let source = format!(
            "import \"std.http\"\nfn handler() {{ return http.text(\"ok\") }}\nfn main() {{\n app = http.service()\n app.get(\"{path}\", handler)\n}}"
        );
        let err = run_err(&source);
        assert!(
            err.contains("invalid http route segment"),
            "unexpected {label} route error: {err}"
        );
    }
}

#[test]
fn std_time_steady_millis_is_monotonic() {
    let source = r#"
import { time } from "std"

fn main(): null! {
    before = time.steady_millis()
    time.sleep(1)?
    after = time.steady_millis()
    if (after < before) {
        panic("steady clock moved backwards")
    }
    return ok(null)
}
"#;

    check_source("inline.ku", source).expect("steady clock api should check");
    run_source("inline.ku", source).expect("steady clock api should run");
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
    let parse_error =
        run_err(r#"fn main(): null! { print(json.parse("{bad}")?) return ok(null) }"#);
    assert!(
        parse_error.contains("expected") || parse_error.contains("json"),
        "unexpected error: {parse_error}"
    );

    let stringify_error = run_err(
        r#"
fn main(): null! {
    value = ok(1)
    print(json.stringify(value)?)
    return ok(null)
}
"#,
    );
    assert!(
        stringify_error.contains("json.stringify does not support result"),
        "unexpected error: {stringify_error}"
    );

    for bad_json in ["1.", "1e", "01"] {
        let source =
            format!(r#"fn main(): null! {{ print(json.parse("{bad_json}")?) return ok(null) }}"#);
        let err = run_err(&source);
        assert!(
            err.contains("digit") || err.contains("leading zero"),
            "unexpected error for {bad_json}: {err}"
        );
    }
}

#[test]
fn http_pg_example_serializes_database_text_and_avoids_dom_html_sinks() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_path = root.join("examples").join("http_pg.ku");
    let source = fs::read_to_string(&source_path).expect("read HTTP + PostgreSQL example");
    check_source(&source_path.to_string_lossy(), &source)
        .expect("HTTP + PostgreSQL example should type-check");
    assert!(source.contains("body = json.stringify({"));
    for unsafe_fragment in ["+ ver +", "+ database +", "+ now +"] {
        assert!(
            !source.contains(unsafe_fragment),
            "database text must not be concatenated into JSON: {unsafe_fragment}"
        );
    }

    let frontend = fs::read_to_string(root.join("examples").join("http_pg_frontend.html"))
        .expect("read HTTP + PostgreSQL frontend");
    assert!(
        !frontend.contains("innerHTML"),
        "database response values must not enter an HTML parsing sink"
    );
    assert!(frontend.contains("replaceChildren"));
    assert!(frontend.contains("textContent"));
}

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

    now:int = time.now()
    millis:int = time.millis()
    print(now <= millis)
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

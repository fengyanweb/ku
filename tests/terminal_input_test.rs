//! Exercise the same input source through the interpreter and generated C.
#![cfg(any(unix, windows))]

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod process_harness;

use process_harness::{run_bounded, TempDir, BUILD_LIMITS, BUILD_TIMEOUT, RUN_TIMEOUT};
use std::{
    fs::{self, File},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const SOURCE: &str = r#"
fn make_prompt(): str {
    println("prompt evaluated")
    return "请输入名字：${}"
}

fn main(): null! {
    name = input(make_prompt())?
    text, number = input(`文本${}，数字${:int}。`)?
    blank = input()?

    try {
        text, number = input(`文本${}，数字${:int}。`)?
        println("unexpected success")
    } catch (problem) {
        println(problem.code)
    }
    last = input()?
    println(`<{name}>`)
    println(`<{text}>`)
    println(number)
    println(`<{blank}>`)
    println(last)

    try {
        println(input()?)
    } catch (problem) {
        println(problem.domain)
        println(problem.code)
    }
    return ok(null)
}
"#;

fn assert_input_output(command: &mut Command, input: &Path) {
    let mut child = command
        .stdin(File::open(input).expect("open redirected test input"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start input fixture");
    let deadline = Instant::now() + RUN_TIMEOUT;
    while child
        .try_wait()
        .expect("check input fixture exit")
        .is_none()
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait().expect("reap timed-out input fixture");
            panic!("input fixture exceeded its deadline: {command:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child
        .wait_with_output()
        .expect("collect input fixture output");
    assert!(output.status.success(), "{command:?}: {:?}", output);
    assert!(output.stderr.is_empty(), "{command:?}: {:?}", output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace("\r\n", "\n"),
        "prompt evaluated\n请输入名字：${}invalid_integer\n< 小明🙂 >\n<0018>\n-18\n<>\n末尾🙂\ninput\neof\n",
        "{command:?}"
    );
}

#[test]
fn input_preserves_lines_and_returns_typed_fields_atomically() {
    let directory = TempDir::new("terminal-input");
    fs::write(directory.path().join("main.ku"), SOURCE).unwrap();
    let input = directory.path().join("stdin.txt");
    fs::write(
        &input,
        " 小明🙂 \r\n0018\r\n -18 \t\r\n\r\n重填\r\n7x\r\n末尾🙂",
    )
    .unwrap();
    let ku = env!("CARGO_BIN_EXE_ku");
    assert_input_output(
        Command::new(ku)
            .current_dir(directory.path())
            .args(["run", "main.ku"]),
        &input,
    );

    let name = if cfg!(windows) {
        "native.exe"
    } else {
        "native"
    };
    let build = run_bounded(
        Command::new(ku).current_dir(directory.path()).args([
            "build",
            "--release",
            "main.ku",
            "-o",
            name,
        ]),
        BUILD_TIMEOUT,
        BUILD_LIMITS,
    )
    .expect("native input fixture build finished");
    let details = String::from_utf8_lossy(&build.stderr);
    if !build.status.success() && details.contains("C compiler not found") {
        assert!(std::env::var_os("GITHUB_ACTIONS").is_none(), "{details}");
        eprintln!("skipping native input execution because no C compiler is available");
        return;
    }
    assert!(build.status.success(), "{build:?}");
    assert_input_output(
        Command::new(directory.path().join(name)).current_dir(directory.path()),
        &input,
    );
}

#[test]
fn input_is_rejected_in_an_async_function() {
    let error = ku::cli::check_source(
        "async_input.ku",
        "async fn main(): null! { value = input()? println(value) return ok(null) }",
    )
    .expect_err("a Task cannot block on terminal input");
    assert!(
        error
            .to_string()
            .contains("input requires synchronous CLI execution"),
        "{error}"
    );
}

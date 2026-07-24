//! End-to-end native tests: compile generated C with the real toolchain
//! (zig/clang/gcc, or MSVC cl.exe via vcvars) and run the produced binary,
//! asserting stdout/exit. When no C compiler is present the tests skip cleanly
//! instead of failing, so they stay green on machines without a toolchain.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ku_binary() -> PathBuf {
    if let Ok(path) = env::var("KU_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_ku") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    let exe = if cfg!(windows) { "ku.exe" } else { "ku" };
    let target_dir = env::var("CARGO_TARGET_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"));
    [
        target_dir.join("debug").join(exe),
        target_dir.join("release").join(exe),
        repo_root().join("target").join("debug").join(exe),
        repo_root().join("target").join("release").join(exe),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("ku binary not found; set KU_BIN or build the ku binary first")
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "ku-native-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Build `entry_rel` (relative to `dir`) into a native binary at `dir/out`.
/// Returns the binary path, or `None` when no C compiler is available (skip).
fn native_build(dir: &Path, entry_rel: &str, out_stem: &str) -> Option<PathBuf> {
    let out = exe_name(out_stem);
    let output = Command::new(ku_binary())
        .current_dir(dir)
        .args(["build", "--native", entry_rel, "-o", &out])
        .output()
        .expect("spawn ku build --native");
    if output.status.success() {
        return Some(dir.join(&out));
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if combined.contains("C compiler not found") {
        eprintln!("skip: no C compiler available for native e2e test");
        return None;
    }
    panic!("ku build --native failed unexpectedly:\n{combined}");
}

fn run_binary(exe: &Path) -> (String, Option<i32>) {
    let output = Command::new(exe)
        .current_dir(exe.parent().unwrap_or_else(|| Path::new(".")))
        .output()
        .unwrap_or_else(|err| panic!("failed to run native binary {}: {err}", exe.display()));
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code(),
    )
}

#[test]
fn native_import_graph_binary_runs_after_sources_removed() {
    let dir = unique_temp_dir("import-graph");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        src.join("math.ku"),
        "fn Add(a:int, b:int): int {\n    return a + b\n}\n",
    )
    .expect("write math.ku");
    fs::write(
        src.join("main.ku"),
        "import { Add } from \"./math.ku\"\n\nfn main(): null! {\n    println(Add(1, 2))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "src/main.ku", "app") else {
        return;
    };

    // Stage 1 acceptance: the binary must not depend on the .ku source paths.
    fs::remove_dir_all(&src).expect("remove sources");
    assert!(!src.exists(), "source dir should be gone");

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.trim(), "3", "expected 3 after removing sources");
    assert_eq!(code, Some(0), "binary should exit cleanly");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kustring_clone_prints_utf8_twice() {
    let dir = unique_temp_dir("kustring-clone");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    a = \"héllo\"\n    b = a.clone()\n    println(a)\n    println(b)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "kustr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "héllo\nhéllo\n",
        "clone must deep-copy and print UTF-8 by length, not NUL"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_nested_array_clone_does_not_double_free() {
    let dir = unique_temp_dir("nested-clone");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    a = [[1, 2], [3, 4]]\n    b = a.clone()\n    println(a[0][0])\n    println(b[1][1])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "nested") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n4\n");
    // Regression: `a[0]` reads an owned element as a borrow; dropping it as if it
    // owned the container's inner pointer used to double-free (0xC0000374).
    assert_eq!(code, Some(0), "reading a[0] must borrow, not double-free");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_push_len_match_interpreter() {
    let dir = unique_temp_dir("array-push");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    nums = [1, 2, 3]\n    more = nums.push(4)\n    println(nums.len())\n    println(more.len())\n    println(more[3])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "push") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // push is immutable: nums stays length 3, the returned array is length 4.
    assert_eq!(stdout.replace('\r', ""), "3\n4\n4\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_try_get_ok_path() {
    let dir = unique_temp_dir("try-get-ok");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    nums = [10, 20, 30]\n    got = nums.try_get(1)?\n    println(got)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "tryget") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "20\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_try_get_out_of_bounds_propagates_err() {
    let dir = unique_temp_dir("try-get-oob");
    // `nums[i]` would abort; `try_get(i)?` returns a recoverable Err that `?`
    // propagates, so the main wrapper exits non-zero instead of crashing.
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    nums = [10, 20, 30]\n    bad = nums.try_get(9)?\n    println(bad)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "trygetoob") else {
        return;
    };

    let (_stdout, code) = run_binary(&exe);
    assert_eq!(code, Some(1), "out-of-bounds try_get must propagate an error");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fail_object_catch_fields_and_finally() {
    let dir = unique_temp_dir("fail-object-catch");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    try {\n        fail {\n            domain: \"test\",\n            code: \"failed\",\n            message: \"boom\"\n        }\n    } catch(err) {\n        println(err.domain)\n        println(err.code)\n        println(err.message)\n    } finally {\n        println(\"cleanup\")\n    }\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "failobj") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "test\nfailed\nboom\ncleanup\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fail_object_propagates_via_question() {
    let dir = unique_temp_dir("fail-object-prop");
    fs::write(
        dir.join("main.ku"),
        "fn Load(): str! {\n    fail {\n        domain: \"fs\",\n        code: \"read_failed\",\n        message: \"cannot read\"\n    }\n}\n\nfn main(): null! {\n    text = Load()?\n    println(text)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "failprop") else {
        return;
    };

    let (_stdout, code) = run_binary(&exe);
    assert_eq!(code, Some(1), "fail must propagate to a non-zero exit");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_return_through_finally_runs_cleanup() {
    let dir = unique_temp_dir("return-finally");
    fs::write(
        dir.join("main.ku"),
        "fn value(flag:bool): int! {\n    try {\n        if (flag) {\n            return ok(7)\n        }\n    } finally {\n        println(\"cleanup\")\n    }\n    return ok(9)\n}\n\nfn main(): null! {\n    v = value(true)?\n    println(v)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "retfinally") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // finally runs even though try returns; the returned 7 flows through it.
    assert_eq!(stdout.replace('\r', ""), "cleanup\n7\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_object_index_strict_read() {
    let dir = unique_temp_dir("object-read");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    obj = { name: \"Ku\", age: 18 }\n    println(obj[\"name\"]?)\n    println(obj[\"age\"]?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "objread") else {
        return;
    };

    // obj[key]? yields a KuValue printed by tag (str -> Ku, int -> 18).
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Ku\n18\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_object_missing_key_and_get_or() {
    let dir = unique_temp_dir("object-missing");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    obj = { name: \"Ku\" }\n    try {\n        v = obj[\"age\"]?\n        println(v)\n    } catch(err) {\n        println(err.code)\n    }\n    println(obj.get_or(\"age\", 99))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "objmiss") else {
        return;
    };

    // Missing key -> Err{code:"missing_key"} caught; get_or returns the default.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "missing_key\n99\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kuvalue_as_int_as_str_chain() {
    let dir = unique_temp_dir("kuvalue-as");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    obj = { age: 18, name: \"Ku\" }\n    n = obj[\"age\"]?.as_int()?\n    s = obj[\"name\"]?.as_str()?\n    println(n)\n    println(s)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "kvas") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "18\nKu\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_write_read_roundtrip() {
    let dir = unique_temp_dir("fs-rt");
    fs::write(
        dir.join("main.ku"),
        "import fs from \"std.fs\"\n\nfn main(): null! {\n    fs.write(\"s7.txt\", \"native fs works\")\n    println(fs.read(\"s7.txt\"))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fsrt") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "native fs works\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_parse_read_and_convert() {
    let dir = unique_temp_dir("json-rt");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    obj = json.parse(\"{\\\"name\\\":\\\"Ku\\\",\\\"age\\\":18}\")\n    println(obj[\"name\"]?.as_str()?)\n    println(obj[\"age\"]?.as_int()?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonrt") else {
        return;
    };

    // json.parse -> KuValue -> obj[key]? -> as_str/as_int, native == interpreter.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Ku\n18\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_array_roundtrip() {
    let dir = unique_temp_dir("json-arr");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    println(json.stringify(json.parse(\"[1,2,3]\")))\n    println(json.stringify(json.parse(\"[{\\\"a\\\":1},{\\\"a\\\":2}]\")))\n    println(json.stringify(json.parse(\"[]\")))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonarr") else {
        return;
    };

    // KuValue KU_ARRAY parse/stringify round-trip: scalars, object arrays, empty.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "[1,2,3]\n[{\"a\":1},{\"a\":2}]\n[]\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_array_element_access() {
    let dir = unique_temp_dir("json-idx");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    a = json.parse(\"[10,20,30]\")\n    println(a[0]?.as_int()?)\n    println(a[2]?.as_int()?)\n    obj = json.parse(\"{\\\"items\\\":[7,8,9]}\")\n    items = obj[\"items\"]?\n    println(items[1]?.as_int()?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonidx") else {
        return;
    };

    // KuValue array int-index `arr[i]?` -> element, including a nested
    // obj["items"]?[i]? read; native matches the interpreter.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "10\n30\n8\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_try_catch_mixed_result_chain() {
    // A try block whose `?` operators unwrap DIFFERENT Result types in one
    // statement — `a[9]?` (KuValue) then `.as_int()?` (int) — share a single
    // KuError-typed error slot, so the out-of-bounds error reaches catch as
    // `index_out_of_bounds`. This regression-guards the fix where the slot
    // was pinned to the first `?`'s Result type and a later differently-typed
    // `?` failed to compile.
    let dir = unique_temp_dir("try-mixed");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    a = json.parse(\"[10,20]\")\n    try {\n        x = a[9]?.as_int()?\n        println(x)\n    } catch (e) {\n        println(e.code)\n    }\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "trymixed") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "index_out_of_bounds\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_literal_no_capture() {
    // Stage 6a: a no-capture closure literal lowers to a lifted `__ku_closure_N`
    // C function reached through an indirect `{invoke, env=NULL}` call.
    let dir = unique_temp_dir("closure-lit");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    f = () => { return 42 }\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closlit") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

/// try/catch/finally return behavior, pinned against the interpreter. Stage 8e
/// added return-type inference for functions declared WITHOUT a `: T` annotation;
/// this locks in that it did not disturb any of the existing paths. Annotated
/// functions skip the inference pass entirely, and every case below is annotated.
///
/// Covers: try completing normally, `return` inside try, `?` returning early out
/// of try into catch, `?` succeeding inside try, `return` inside catch, finally
/// completing normally, and finally running without changing the enclosing
/// function's return type when try returned.
const TRY_FINALLY_SOURCE: &str = concat!(
    "fn boom(): int! {\n    fail { domain: \"t\", code: \"b\", message: \"m\" }\n}\n\n",
    "fn ok_src(): int! {\n    return ok(7)\n}\n\n",
    "fn a(): int {\n    x = 0\n    try {\n        x = 1\n    } catch (e) {\n        x = 2\n    }\n    return x\n}\n\n",
    "fn b(): int {\n    try {\n        return 10\n    } catch (e) {\n    }\n    return 99\n}\n\n",
    "fn c(): int {\n    try {\n        v = boom()?\n        return v\n    } catch (e) {\n        return 30\n    }\n    return 31\n}\n\n",
    "fn d(): int {\n    try {\n        v = ok_src()?\n        return v + 1\n    } catch (e) {\n        return 40\n    }\n    return 41\n}\n\n",
    "fn e_fin(): int {\n    r = 0\n    try {\n        r = 50\n    } finally {\n        println(\"fin-e\")\n    }\n    return r\n}\n\n",
    "fn f_fin(): int {\n    try {\n        return 60\n    } finally {\n        println(\"fin-f\")\n    }\n    return 61\n}\n\n",
    "fn main(): null! {\n    println(a())\n    println(b())\n    println(c())\n    println(d())\n    println(e_fin())\n    println(f_fin())\n    return ok(null)\n}\n",
);

#[test]
fn native_try_finally_return_paths_match_interpreter() {
    let dir = unique_temp_dir("try-finally");
    fs::write(dir.join("main.ku"), TRY_FINALLY_SOURCE).expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "tryfin") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // Verified identical under `ku run` on the interpreter.
    assert_eq!(
        stdout.replace('\r', ""),
        "1\n10\n30\n8\nfin-e\n50\nfin-f\n60\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_unannotated_function_with_try_used_as_value() {
    // The one shape Stage 8e's inference pass actually touches: no return
    // annotation AND a try in the body. The `return` outside the try gives the
    // pass a concrete type, so the function is usable as a value; before, it
    // lowered as `void` and could not be emitted as a closure at all.
    let dir = unique_temp_dir("try-noann");
    fs::write(
        dir.join("main.ku"),
        "fn t() {\n    try {\n        return 5\n    } catch (e) {\n    }\n    return 6\n}\n\nfn main(): null! {\n    f = t\n    println(f())\n    println(t())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "trynoann") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "5\n5\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_unannotated_return_type_is_inferred_from_body() {
    // Stage 8e: a top-level function with no `: T` return annotation gets its
    // return type inferred from the body, exactly like the checker does. Two
    // things are pinned here:
    //   * The function is usable as a *value* at all -- an unannotated return used
    //     to lower as `void`, so `f = pick` produced a `Closure { ret: void }` the
    //     C backend could not emit.
    //   * `null` is the identity element when folding the body's returns (the
    //     checker's merge_return_types), so a body that returns `null` on one path
    //     and `int` on another infers `int`. Taking the first return instead would
    //     infer `null` here and disagree with the checker.
    let dir = unique_temp_dir("infer-return");
    fs::write(
        dir.join("main.ku"),
        "fn pick(flag: bool) {\n    if (flag) {\n        return null\n    }\n    return 5\n}\n\nfn main(): null! {\n    f = pick\n    println(f(false))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "inferret") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "5\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

/// Owned-field move coverage for `c_move_place` (Stage 8e). Reading an owned field
/// in value position moves it, so the source must be cleared — otherwise the
/// owning struct's own drop frees the buffer the moved value now holds. Each
/// function below routes an owned field out through a different control-flow exit
/// (plain return, early return, Result ok, catch, finally) so that every cleanup
/// path is exercised in one binary; a missed clear shows up as a double free.
// c_move_place is the backend safety net for owned values read in value position.
// The checker's consume_expr already forbids *directly* moving a plain owned field
// or indexed element (it demands `.clone()`), so this pins the shapes that DO reach
// codegen as a move: a `.clone()`d field (its own fresh allocation), and -- the
// shape that slipped past the checker into a real double free -- an HTTP handler
// returning `req.body`, covered end-to-end in native_http_test.rs. Here we drive
// the language-level paths that are legal, through every control-flow exit, so a
// missing clear on any of them double-frees instead of exiting 0.
const FIELD_MOVE_SOURCE: &str = concat!(
    "struct Holder {\n    name: str\n}\n\n",
    // an owned COPY (clone) returned out of the function: the source struct still
    // owns its field and must drop it exactly once when the function returns.
    "fn take_clone(h: Holder): str {\n    return h.name.clone()\n}\n\n",
    // read-only use through concatenation must NOT be treated as a move: the field
    // is still owned by the struct and must drop exactly once.
    "fn peek(h: Holder): str {\n    return \"[\" + h.name + \"]\"\n}\n\n",
    // early return before touching the field still drops the struct exactly once.
    "fn early(h: Holder, skip: bool): str {\n    if (skip) {\n        return \"skipped\"\n    }\n    return h.name.clone()\n}\n\n",
    // clone moved out through a Result payload.
    "fn take_result(h: Holder): str! {\n    return ok(h.name.clone())\n}\n\n",
    // clone moved out from inside a try, with a finally running afterwards.
    "fn take_try(h: Holder): str {\n    try {\n        return h.name.clone()\n    } finally {\n        println(\"fin\")\n    }\n    return \"unreachable\"\n}\n\n",
    // a whole owned local (a struct) moved out of the function as a value.
    "fn passthrough(h: Holder): Holder {\n    return h\n}\n\n",
    "fn make(n: str): Holder {\n    return Holder{ name: n }\n}\n\n",
    "fn main(): null! {\n",
    "    println(take_clone(make(\"alpha\")))\n",
    "    println(peek(make(\"gamma\")))\n",
    "    println(early(make(\"delta\"), true))\n",
    "    println(early(make(\"epsilon\"), false))\n",
    "    println(take_result(make(\"zeta\"))?)\n",
    "    println(take_try(make(\"eta\")))\n",
    "    println(passthrough(make(\"theta\")).name)\n",
    "    return ok(null)\n}\n",
);

#[test]
fn native_nested_field_move_preserves_siblings() {
    // Moving a nested field (`c.user.name`) must move only that leaf, not the whole
    // intermediate struct — the sibling `c.user.email` and `c.host` must survive
    // with their values intact.
    let dir = unique_temp_dir("nested-move");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct User { name: str, email: str }\n",
            "struct Config { user: User, host: str }\n\n",
            "fn main() {\n",
            "  dom = \"example\"\n",
            "  c = Config { user: User { name: \"alice\", email: dom + \".com\" }, host: \"localhost\" }\n",
            "  n = c.user.name\n",
            "  println(n)\n",
            "  println(c.user.email)\n",
            "  println(c.host)\n}\n",
        ),
    )
    .expect("write main.ku");
    let Some(exe) = native_build(&dir, "main.ku", "nestedmove") else { return };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "alice\nexample.com\nlocalhost\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_match_binding_used_more_than_once_is_not_re_moved() {
    // A match-bound owned payload must be extracted once; using the binding twice
    // must read the same value, not re-move an already-cleared enum slot.
    let dir = unique_temp_dir("match-multi");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Payload { name: str, note: str }\n",
            "enum Box { Full(p: Payload)  Empty }\n",
            "fn build(a: str, b: str): Payload { return Payload { name: a + \"-tag\", note: b + \"-tag\" } }\n\n",
            "fn main() {\n",
            "  b = Box.Full(build(\"alice\", \"hello\"))\n",
            "  text = match b {\n    Box.Full(p) => p.name + \":\" + p.note\n    Box.Empty => \"empty\"\n  }\n",
            "  println(text)\n}\n",
        ),
    )
    .expect("write main.ku");
    let Some(exe) = native_build(&dir, "main.ku", "matchmulti") else { return };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "alice-tag:hello-tag\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_void_returning_function_call_compiles() {
    // A call to a user function with no return type must not emit `void t0 = f()`.
    let dir = unique_temp_dir("void-call");
    fs::write(
        dir.join("main.ku"),
        "fn sink(v: str) { println(v) }\nfn main() {\n    sink(\"literal\")\n    println(\"after\")\n}\n",
    )
    .expect("write main.ku");
    let Some(exe) = native_build(&dir, "main.ku", "voidcall") else { return };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "literal\nafter\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_clone_is_deep_not_aliasing() {
    // `.clone()` of a struct with an owned string field must DEEP-clone the field,
    // not shallow-copy it. A shallow copy aliases the buffer, so moving the field
    // out of both the original and the clone frees it twice.
    let dir = unique_temp_dir("struct-clone");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U { name: str }\n\n",
            "fn main() {\n",
            "  base = \"hel\"\n",
            "  u = U{ name: base + \"lo\" }\n",
            "  v = u.clone()\n",
            "  a = u.name\n",
            "  b = v.name\n",
            "  println(a)\n",
            "  println(b)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structclone") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hello\nhello\n");
    assert_eq!(code, Some(0)); // a double free would abort with 0xC0000374
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_literal_consumes_its_owned_field_source() {
    // Building a struct from an owned local must MOVE the value into the field
    // (clearing the source), not shallow-copy it — otherwise the source local and
    // the field both own the same buffer and both free it.
    let dir = unique_temp_dir("struct-literal");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U { name: str }\n\n",
            "fn main() {\n",
            "  base = \"hel\"\n",
            "  s = base + \"lo\"\n",
            "  u = U{ name: s }\n",
            "  a = u.name\n",
            "  println(a)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structlit") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hello\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_owned_field_does_not_leak_on_drop() {
    // A struct's owned fields must be deep-dropped when it goes out of scope; a
    // no-op drop would leak them. This just pins that such a program runs and
    // exits cleanly (ASan/CRT leak runs are done separately).
    let dir = unique_temp_dir("struct-drop");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U { name: str, tag: str }\n\n",
            "fn build(n: str): U {\n    return U{ name: n, tag: \"t\".clone() }\n}\n\n",
            "fn main() {\n",
            "  u = build(\"kept\".clone())\n",
            "  println(u.name)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structdrop") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "kept\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_enum_payload_is_moved_in_not_double_freed() {
    // The enum literal takes ownership of its payload argument. The construction
    // used to shallow-copy the argument (leaving the source binding/temp still
    // owning the same heap buffer), so extracting the payload via `match` and then
    // dropping both the extracted value and the un-cleared source double-freed the
    // string. The fix moves-and-clears the argument into the payload.
    let dir = unique_temp_dir("enum-payload");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "enum Box {\n  Full(value: str)\n  Empty\n}\n\n",
            "fn main() {\n",
            "  n = \"world\"\n",
            "  b = Box.Full(\"hello \" + n)\n",
            "  msg = match b {\n    Box.Full(value) => value\n    Box.Empty => \"empty\"\n  }\n",
            "  println(msg)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "enumpayload") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hello world\n");
    // A double free aborts with STATUS_HEAP_CORRUPTION instead of exiting 0.
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_partial_field_move_keeps_sibling_and_drops_safely() {
    // The checker now allows moving a single owned struct field out (`n = u.name`)
    // while keeping the siblings usable. Native must execute that safely: the move
    // clears `u.name`, so when `u` is later dropped only the un-moved fields are
    // freed (no double free of the moved-out string). This is a real field MOVE,
    // not a `.clone()`.
    let dir = unique_temp_dir("partial-move");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U {\n    name: str\n    tag: str\n}\n\n",
            "fn make(): U {\n    return U{ name: \"kept\".clone(), tag: \"also\".clone() }\n}\n\n",
            "fn main(): null! {\n",
            "    u = make()\n",
            "    n = u.name\n",     // move the name field out
            "    println(n)\n",     // moved value still owned here
            "    println(u.tag)\n", // sibling still usable
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "partialmove") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "kept\nalso\n");
    // A double free of the moved-out field aborts (STATUS_HEAP_CORRUPTION) rather
    // than exiting 0, so the exit code is the real assertion.
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_primitive_array_fields() {
    // A struct may hold `[int]`/`[bool]`/`[str]` fields: the primitive array ABI is
    // emitted before the struct layout so it embeds by value, and the struct's deep
    // clone/drop recurses into the array (verified: clone is independent, no leak or
    // double free — the exit code is the real assertion for the cloned/dropped run).
    let dir = unique_temp_dir("struct-array-field");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Rec { tags: [str], flags: [bool], nums: [int] }\n",
            "fn describe(r: Rec): int { return r.nums.len() + r.tags.len() }\n",
            "fn main(): null! {\n",
            "    r = Rec { tags: [\"a\", \"b\"], flags: [true, false], nums: [1, 2, 3, 4] }\n",
            "    println(r.tags[0])\n",           // a
            "    println(str(r.flags[1]))\n",     // false
            "    println(r.nums.len())\n",        // 4
            "    c = r.clone()\n",                // deep clone of the array fields
            "    println(c.nums[3])\n",           // 4
            "    println(describe(c))\n",         // 4 + 2 = 6
            "    println(r.tags[1])\n",           // b — original still intact after clone
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structarr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "a\nfalse\n4\n4\n6\nb\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_array_of_struct_field() {
    // A struct may hold an array of another struct (`[Worker]`), including a nested
    // array field on the element (`tags: [str]`). The layered emission resolves the
    // struct↔array cycle (forward-declared tags + array typedefs before the struct
    // bodies, forward-declared array helpers), and deep clone/drop recurse through
    // both levels — no leak or double free (exit code is the assertion).
    let dir = unique_temp_dir("struct-array-of-struct");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Worker { id: int, tags: [str] }\n",
            "struct Team { members: [Worker], tag: str }\n",
            "fn main(): null! {\n",
            "    t = Team { members: [ Worker{id: 7, tags: [\"x\", \"y\"]}, Worker{id: 9, tags: [\"z\"]} ], tag: \"T\" }\n",
            "    println(t.members.len())\n",         // 2
            "    println(t.members[0].id)\n",         // 7
            "    println(t.members[0].tags[1])\n",    // y
            "    c = t.clone()\n",                     // deep clone through both levels
            "    println(c.members[1].tags[0])\n",    // z
            "    println(t.members[1].id)\n",         // 9 — original intact
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "arrofstruct") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "2\n7\ny\nz\n9\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_template_string_interpolation_matches_interpreter() {
    // Backtick templates must be interpolated in native, not emitted as a literal
    // with `{placeholders}`. Each `{expr}` becomes str(expr); `\{`/`\}` are literal
    // braces.
    let dir = unique_temp_dir("template-string");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main(): null! {\n",
            "    name = \"Ku\"\n",
            "    n = 30\n",
            "    println(`Hello {name} {n}`)\n",
            "    println(`sum={n + n} done`)\n",
            "    println(`brace \\{ x \\}`)\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "template") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Hello Ku 30\nsum=60 done\nbrace { x }\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_string_literal_with_non_ascii_bytes_is_not_corrupted() {
    // A string literal carrying a non-printable byte (U+00A0 NBSP) must survive to
    // the C source intact. Rust's Debug `\u{a0}` escape is invalid C and MSVC would
    // mangle it to the ASCII text `u{a0}`, corrupting len/contains/println.
    let dir = unique_temp_dir("nonascii-literal");
    // "x" + U+00A0 (0xC2 0xA0) + "y"
    let src = "fn main() {\n    s = \"x\u{a0}y\"\n    println(s.len())\n    println(str(s.contains(\"u\")))\n}\n";
    fs::write(dir.join("main.ku"), src).expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "nbsp") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // 3 codepoints (x, NBSP, y); contains("u") is false — not the mangled "u{a0}".
    assert_eq!(stdout.replace('\r', ""), "3\nfalse\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_pushing_an_owned_literal_does_not_leak() {
    // array.push clones its value into the new array (the source stays usable, like
    // the interpreter). A pushed fresh struct literal used to never be dropped,
    // leaking its owned fields; it must now be materialized and freed. The run
    // completing with the right output (and, under a leak checker, zero leaks) is the
    // assertion — here we at least confirm it runs correctly and the source is intact.
    let dir = unique_temp_dir("push-owned-literal");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct W { id: int, tags: [str] }\n",
            "fn main(): null! {\n",
            "    xs = [ W{ id: 0, tags: [\"seed\"] } ]\n",
            "    i = 0\n",
            "    while (i < 20) {\n",
            "        ys = xs.clone()\n",
            "        r = ys.push(W{ id: i, tags: [\"a\" + \"x\", \"b\" + \"y\"] })\n",
            "        i = i + r.len() - 1\n",
            "    }\n",
            "    println(i)\n",
            "    println(xs.len())\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "pushowned") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "20\n1\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_enum_field_topological() {
    // A struct may hold an enum field, and the enum may carry a struct payload — a
    // struct→enum→struct value-embedding chain. The unified topological layout pass
    // emits Point, then Shape, then Figure so every by-value type is complete before
    // its user. Deep clone/drop recurse through the enum payload; no leak/double free.
    let dir = unique_temp_dir("struct-enum-field");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Point { x: int, y: int }\n",
            "enum Shape { Dot, Circle(p: Point), Tag(s: str) }\n",
            "struct Figure { shape: Shape, name: str }\n",
            "fn main(): null! {\n",
            "    f = Figure { shape: Shape.Tag(\"hi\" + \"!\"), name: \"fig\" }\n",
            "    c = f.clone()\n",
            "    match c.shape { Shape.Tag(s) => println(s)  Shape.Dot => println(\"d\")  Shape.Circle(p) => println(p.x) }\n",
            "    println(f.name)\n",
            "    g = Figure { shape: Shape.Circle(Point{x: 4, y: 9}), name: \"g\" }\n",
            "    match g.shape { Shape.Circle(p) => println(p.y)  Shape.Dot => println(\"d\")  Shape.Tag(s) => println(s) }\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structenum") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hi!\nfig\n9\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_string_methods_match_interpreter() {
    // len counts Unicode scalar values (café = 4), contains/starts_with/ends_with
    // are byte substring tests (empty needle is always true), replace is a
    // non-overlapping all-occurrences swap, and slice is char-indexed and returns
    // a Result. All must be byte-identical to the interpreter.
    let dir = unique_temp_dir("string-methods");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main(): null! {\n",
            "    a = \"hello world\"\n",
            "    println(a.len())\n",                    // 11
            "    println(str(a.contains(\"world\")))\n",  // true
            "    println(str(a.contains(\"\")))\n",       // true
            "    println(str(a.starts_with(\"hell\")))\n",// true
            "    println(str(a.ends_with(\"rld\")))\n",   // true
            "    println(a.replace(\"o\", \"0\"))\n",     // hell0 w0rld
            "    println(a.replace(\"\", \"-\"))\n",      // -h-e-l-l-o- -w-o-r-l-d-
            "    b = \"café\"\n",
            "    println(b.len())\n",                     // 4
            "    println(a.slice(0, 5)?)\n",              // hello
            "    println(b.slice(0, 4)?)\n",              // café
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "strmethods") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "11\ntrue\ntrue\ntrue\ntrue\nhell0 w0rld\n-h-e-l-l-o- -w-o-r-l-d-\n4\nhello\ncafé\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_string_slice_out_of_bounds_returns_matching_error() {
    // The recoverable slice error must carry the same domain/code/message as the
    // interpreter so a caught error reads identically.
    let dir = unique_temp_dir("string-slice-err");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main() {\n",
            "    try {\n",
            "        r = \"hello\".slice(0, 100)?\n",
            "        println(r)\n",
            "    } catch (e) {\n",
            "        println(e.domain + \"/\" + e.code + \"/\" + e.message)\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "sliceerr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "string/slice_error/string.slice end 100 out of bounds for length 5\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_str_builtin_matches_interpreter_for_primitives() {
    // `str(x)` mirrors the interpreter's `value.to_string()`: int in decimal,
    // bool as true/false, a string identity (borrowed, so the source stays live),
    // and it composes with `+` so an int can be built into a larger string — the
    // gap that blocked the acceptance tool.
    let dir = unique_temp_dir("str-builtin");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main(): null! {\n",
            "    n = 42\n",
            "    println(str(n))\n",       // 42
            "    println(str(0 - 7))\n",   // -7
            "    println(str(true))\n",    // true
            "    println(str(false))\n",   // false
            "    println(str(\"hi\"))\n",  // hi
            "    line = \"age=\" + str(n) + \"!\"\n",
            "    println(line)\n",         // age=42!
            "    println(n)\n",            // 42 — str(n) did not consume n
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "strbuiltin") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "42\n-7\ntrue\nfalse\nhi\nage=42!\n42\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_non_consuming_field_read_leaves_the_field_intact() {
    // Regression (R3): a value-position field read used to lower as move-and-clear,
    // so `println(u.name)` emptied the field and the second read printed nothing,
    // and `u.name.clone()` cleared the source it was supposed to copy. A read must
    // borrow: native output must match the interpreter (both print the value twice).
    let dir = unique_temp_dir("field-read-borrow");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Inner { tag: str }\n",
            "struct Outer { inner: Inner, label: str }\n",
            "fn main(): null! {\n",
            "    o = Outer{ inner: Inner{ tag: \"t\" + \"ag\" }, label: \"L\" + \"bl\" }\n",
            "    println(o.inner.tag)\n",      // read a nested field...
            "    println(o.inner.tag)\n",      // ...twice; the second must still work
            "    c = o.label.clone()\n",       // clone must not clear the source
            "    println(c)\n",
            "    println(o.label)\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fieldread") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "tag\ntag\nLbl\nLbl\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_reboxed_each_iteration_releases_prior_cells() {
    // Regression (R8): a captured local re-boxed each loop iteration was CellNew'd
    // over the previous box without releasing it, leaking a cell (and its captured
    // string) per iteration — CRT reported 398 leaked blocks over 200 loops, now 0.
    // The fix releases the prior cell before overwriting; over-releasing would
    // double-free and abort, so a clean exit 0 across many iterations is the guard.
    let dir = unique_temp_dir("closure-loop");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main(): null! {\n",
            "    i = 0\n",
            "    while (i < 50) {\n",
            "        u = \"row\" + \"x\"\n",
            "        g = () => { return u + \"?\" }\n",
            "        println(g())\n",
            "        i = i + 1\n",
            "    }\n",
            "    println(\"end\")\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closureloop") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    let normalized = stdout.replace('\r', "");
    let lines: Vec<&str> = normalized.lines().collect();
    assert_eq!(lines.len(), 51, "50 loop lines + end");
    assert!(lines.iter().take(50).all(|l| *l == "rowx?"));
    assert_eq!(lines[50], "end");
    assert_eq!(code, Some(0), "re-boxing must not double-free");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_owned_field_move_across_every_exit_path() {
    let dir = unique_temp_dir("field-move-paths");
    fs::write(dir.join("main.ku"), FIELD_MOVE_SOURCE).expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "movepaths") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "alpha\n[gamma]\nskipped\nepsilon\nzeta\nfin\neta\ntheta\n"
    );
    // A missed clear double-frees and aborts (STATUS_HEAP_CORRUPTION) instead of
    // exiting 0, so the exit code is the real assertion here.
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_owned_struct_field_moved_into_a_value_is_not_double_freed() {
    // Stage 8e: reading an owned field in value position MOVES it, so the source
    // field must be cleared. It used to be copied, leaving the struct's own drop
    // to free the same buffer the moved value now owned -- a double free that
    // corrupted the heap (this is the `http.text(req.body)` handler shape from
    // cli_v001, which no native test had ever reached).
    let dir = unique_temp_dir("field-move");
    fs::write(
        dir.join("main.ku"),
        "struct Holder {\n    name: str\n}\n\nfn take(h: Holder): str {\n    return h.name\n}\n\nfn main(): null! {\n    h = Holder{ name: \"kept\" }\n    println(take(h))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fieldmove") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "kept\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_top_level_function_value() {
    // Stage 6a: a top-level function used as a value lowers to a `{name__thunk,
    // NULL}` closure and is invoked indirectly, matching the interpreter.
    let dir = unique_temp_dir("fn-value");
    fs::write(
        dir.join("main.ku"),
        "fn add(x: int): int {\n    return x + 1\n}\n\nfn main(): null! {\n    g = add\n    println(g(3))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fnvalue") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "4\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_function_value_typed_binding_multi_call() {
    // Stage 6a: a typed function binding `f: fn(): int = Answer` lowers the
    // `fn(): int` annotation to the same closure type as the value, and calling
    // it twice does not consume it.
    let dir = unique_temp_dir("fn-typed");
    fs::write(
        dir.join("main.ku"),
        "fn Answer(): int {\n    return 42\n}\n\nfn main(): null! {\n    f: fn(): int = Answer\n    println(f())\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fntyped") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_function_value_as_parameter() {
    // Stage 6a: a function value flows through a `fn(int,int): int` parameter,
    // is invoked indirectly inside the callee, and stays usable after being
    // passed (no-capture closures are copied by value, env=NULL).
    let dir = unique_temp_dir("fn-param");
    fs::write(
        dir.join("main.ku"),
        "fn Add(a: int, b: int): int {\n    return a + b\n}\n\nfn Apply(op: fn(int, int): int, a: int, b: int): int {\n    return op(a, b)\n}\n\nfn main(): null! {\n    op: fn(int, int): int = Add\n    println(op(1, 2))\n    println(Apply(op, 3, 4))\n    println(op(5, 6))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fnparam") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n7\n11\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_shared_cell() {
    // Stage 6b: a captured Copy local is boxed into a ref-counted cell shared by
    // the closure and the enclosing scope; the closure mutates it and the outer
    // scope observes the change (counter -> 1, 2; outer count == 2).
    let dir = unique_temp_dir("cap-cell");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    count = 0\n    inc = () => { count = count + 1  return count }\n    println(inc())\n    println(inc())\n    println(count)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capcell") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_sees_outer_mutation() {
    // Stage 6b: capture is by reference (shared cell), not a value snapshot, so a
    // mutation of the outer variable made after the closure is built is visible
    // when the closure later reads it.
    let dir = unique_temp_dir("cap-see");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    x = 1\n    f = () => { return x }\n    x = 99\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capsee") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "99\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_early_return() {
    // Stage 6b R2 guard: a boxed cell released on every return path must not
    // double-free; each control-flow path reaches exactly one return.
    let dir = unique_temp_dir("cap-ret");
    fs::write(
        dir.join("main.ku"),
        "fn pick(flag: bool): int! {\n    n = 0\n    bump = () => { n = n + 1  return n }\n    if (flag) {\n        x = bump()\n        return ok(x)\n    }\n    y = bump()\n    z = bump()\n    return ok(z)\n}\n\nfn main(): null! {\n    println(pick(true)?)\n    println(pick(false)?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capret") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_bool_println() {
    // Booleans (literals and comparison results) print as `true`/`false`,
    // matching the interpreter rather than the numeric `1`/`0`.
    let dir = unique_temp_dir("bool-print");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    println(true)\n    println(false)\n    println(1 < 2)\n    println(2 < 1)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "boolprint") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "true\nfalse\ntrue\nfalse\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_call_depth_guard_matches_interpreter() {
    // Stage 6f: deep/infinite recursion reports "maximum function call depth
    // exceeded" and exits cleanly (code 1) instead of a native stack-overflow
    // crash, matching the interpreter's MAX_CALL_DEPTH. A shallow recursion runs.
    let dir = unique_temp_dir("depth-guard");
    fs::write(
        dir.join("shallow.ku"),
        "fn rec(n: int): int {\n    if (n <= 0) { return 0 }\n    return rec(n - 1)\n}\n\nfn main(): null! {\n    println(rec(10))\n    return ok(null)\n}\n",
    )
    .expect("write shallow.ku");
    fs::write(
        dir.join("deep.ku"),
        "fn rec(n: int): int {\n    if (n <= 0) { return 0 }\n    return rec(n - 1)\n}\n\nfn main(): null! {\n    println(rec(1000))\n    return ok(null)\n}\n",
    )
    .expect("write deep.ku");

    if let Some(exe) = native_build(&dir, "shallow.ku", "depthshallow") {
        let (stdout, code) = run_binary(&exe);
        assert_eq!(stdout.replace('\r', ""), "0\n");
        assert_eq!(code, Some(0));
    }
    if let Some(exe) = native_build(&dir, "deep.ku", "depthdeep") {
        let (_stdout, code) = run_binary(&exe);
        // Clean guarded exit, not a stack-overflow crash (127 / access violation).
        assert_eq!(code, Some(1), "deep recursion must exit cleanly, not crash");
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_call_depth_counter_no_drift_on_fail_paths() {
    // Regression: every function exit path — including `fail`, `?` propagation
    // and catch — must decrement the thread-local call-depth counter. Otherwise
    // sequential fail-and-catch calls drift it up and spuriously trip the guard.
    // rec(400) with a fail-then-catch helper at every level stays well under 512
    // active frames, so it must succeed.
    let dir = unique_temp_dir("depth-drift");
    fs::write(
        dir.join("main.ku"),
        "fn helper(): int! {\n    fail { domain: \"x\", code: \"boom\", message: \"z\" }\n}\n\nfn rec(n: int): int {\n    if (n <= 0) { return 0 }\n    try {\n        v = helper()?\n        return v\n    } catch (e) {\n    }\n    return rec(n - 1)\n}\n\nfn main(): null! {\n    println(rec(400))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "depthdrift") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "0\n");
    assert_eq!(code, Some(0), "fail/? exit paths must decrement; no counter drift");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_str_shared() {
    // Stage 6c-str: a captured owned str lives in a shared cell; rebinding the
    // outer variable is visible to the closure, which borrows the cell on read
    // (the `prefix + name` concat borrows `prefix`, no implicit clone).
    let dir = unique_temp_dir("cap-str");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    prefix = \"Hello \"\n    greet = (name: str) => { return prefix + name }\n    prefix = \"Bye \"\n    println(greet(\"Ku\"))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capstr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Bye Ku\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_str_owned_heap_reassign() {
    // Stage 6c-str: reassigning a captured owned str drops the old heap buffer
    // and moves the new one in; the self-read `s = s + "e"` reads before the
    // old value is dropped. `.clone()` returns an owned copy. No double-free.
    let dir = unique_temp_dir("cap-heap");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    s = \"a\" + \"b\"\n    show = () => { return s.clone() }\n    println(show())\n    s = \"c\" + \"d\"\n    println(show())\n    s = s + \"e\"\n    println(show())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capheap") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "ab\ncd\ncde\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_array_borrowed_read() {
    // Stage 6c-array: a closure captures an owned array through a shared cell and
    // borrows it on read (`xs.len()`), no clone/drop; native matches the
    // interpreter (`3`).
    let dir = unique_temp_dir("cap-arr");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    xs = [1, 2, 3]\n    f = () => { return xs.len() }\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "caparr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_array_reassign_visible() {
    // Stage 6c-array: rebinding the captured array drops the old heap buffer and
    // moves the new one into the shared cell; the closure sees the new length
    // (`4`). No double-free (the old buffer is dropped exactly once).
    let dir = unique_temp_dir("cap-arr-reassign");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    xs = [1, 2]\n    f = () => { return xs.len() }\n    xs = [9, 9, 9, 9]\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "caparrre") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "4\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_object_borrowed_read_and_reassign() {
    // Stage 6c-object: a closure captures an owned object through a shared cell
    // and borrows it on read (`get_or`), no clone/drop of the object. Rebinding
    // the object is visible to the closure (`1` then `7`); the old object is
    // dropped exactly once.
    let dir = unique_temp_dir("cap-obj");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    o = {\"a\": 1}\n    g = () => { return o.get_or(\"a\", null) }\n    println(g())\n    o = {\"a\": 7}\n    println(g())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capobj") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n7\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_array_clone_returns_owned() {
    // Stage 6c-array: `.clone()` on a captured array borrows the cell and produces
    // a fresh owned array that can be returned/stored; native matches interp.
    let dir = unique_temp_dir("cap-arr-clone");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    xs = [5, 6, 7]\n    f = () => { return xs.clone() }\n    ys = f()\n    println(ys.len())\n    println(ys[2])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "caparrcl") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n7\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_param_inferred_from_typed_binding() {
    // A/G: a typed binding `greet: fn(str): str` supplies the type of the
    // otherwise unannotated closure parameter `name`; native output matches the
    // interpreter (`Hello Ku`).
    let dir = unique_temp_dir("closure-typed-binding");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    greet: fn(str): str = (name) => { return \"Hello \" + name }\n    println(greet(\"Ku\"))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closbind") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Hello Ku\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_param_inferred_from_higher_order_parameter() {
    // B/G: a higher-order parameter `op: fn(int): int` supplies the type of the
    // unannotated closure parameter `x`; `Apply((x) => x + 1, 41)` is 42 both
    // natively and in the interpreter.
    let dir = unique_temp_dir("closure-hof-param");
    fs::write(
        dir.join("main.ku"),
        "fn Apply(op: fn(int): int, v: int): int {\n    return op(v)\n}\n\nfn main(): null! {\n    println(Apply((x) => x + 1, 41))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closhof") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_escape_factory_counter() {
    // Stage 6e: a factory returns a capturing closure whose env (the boxed `n`
    // cell) escapes the factory's frame. The returned closure keeps mutating its
    // own cell across calls (1, 2); the env is ref-counted so it outlives the
    // factory without a double-free.
    let dir = unique_temp_dir("closure-escape-factory");
    fs::write(
        dir.join("main.ku"),
        "fn make_counter(): fn(): int {\n    n = 0\n    return () => { n = n + 1  return n }\n}\n\nfn main(): null! {\n    c = make_counter()\n    println(c())\n    println(c())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "escfactory") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_escape_two_independent_counters() {
    // Stage 6e: two factory calls produce two closures over *separate* cells, so
    // their counts are independent (1, 2 for the first; 1 for the second). Each
    // env is released exactly once.
    let dir = unique_temp_dir("closure-two-counters");
    fs::write(
        dir.join("main.ku"),
        "fn make_counter(): fn(): int {\n    n = 0\n    return () => { n = n + 1  return n }\n}\n\nfn main(): null! {\n    a = make_counter()\n    b = make_counter()\n    println(a())\n    println(a())\n    println(b())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "twocounters") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n1\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_clone_shares_captured_cell() {
    // Stage 6e-2: `.clone()` on a capturing closure bumps the env refcount and
    // shares the same cell (it is not deep-copied). `f()` then `g()` observe the
    // same counter (1 then 2). No double-free (env released once per owner).
    let dir = unique_temp_dir("closure-clone-shared");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    n = 0\n    f = () => { n = n + 1  return n }\n    g = f.clone()\n    println(f())\n    println(g())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "cloneshared") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_stored_in_typed_array() {
    // Stage 6e-3: a capturing closure is moved into a `[fn(): int]` array and
    // invoked through `fns[0]()`. The array owns the closure's env and releases
    // it on drop (no leak, no double-free).
    let dir = unique_temp_dir("closure-typed-array");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    n = 10\n    f = () => { n = n + 1  return n }\n    fns: [fn(): int] = [f]\n    println(fns[0]())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closarray") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "11\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_stored_in_dynamic_object() {
    // Stage 6e-4: a capturing closure is boxed into a dynamic object as a
    // `KU_FUNCTION` KuValue. Retrieving it with `get_or` clones the KuValue
    // (env retained) and prints `<function>`, matching the interpreter. Both the
    // object and the retrieved value release the env, so it is freed once.
    let dir = unique_temp_dir("closure-dynamic-object");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    n = 100\n    f = () => { n = n + 1  return n }\n    o = { \"handler\": f }\n    g = o.get_or(\"handler\", null)\n    println(g)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closobject") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "<function>\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_argument_is_borrowed_not_moved() {
    // Stage 6d: passing a capturing closure to a higher-order function borrows it
    // (a plain struct copy sharing the env); the callee does not release it, so
    // the caller's binding stays live for a later direct call. `CallTwice(f)`
    // yields 3 (1+2) and the following `f()` yields 3, matching the interpreter.
    let dir = unique_temp_dir("closure-borrow-arg");
    fs::write(
        dir.join("main.ku"),
        "fn CallTwice(op: fn(): int): int {\n    return op() + op()\n}\n\nfn main(): null! {\n    n = 0\n    f = () => { n = n + 1  return n }\n    println(CallTwice(f))\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "borrowarg") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n3\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_returned_from_parameter_is_ref_counted() {
    // Stage 6d soundness: a function receives a capturing closure by retain (the
    // callee owns its own env reference) and returns it. The returned closure and
    // the caller's original binding then share the env, each releasing it once —
    // no double-free (regression guard for pass-by-retain of function arguments).
    let dir = unique_temp_dir("closure-return-param");
    fs::write(
        dir.join("main.ku"),
        "fn id(op: fn(): int): fn(): int {\n    return op\n}\n\nfn main(): null! {\n    n = 0\n    f = () => { n = n + 1  return n }\n    g = id(f)\n    println(g())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "retparam") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_self_recursion() {
    // Stage 6f: a local named function `fn fact(...)` defined inside `main` is
    // lifted like a closure; a self-recursive call reuses the running env by
    // calling the lifted body directly (no self-capture, no RC cycle). fact(5)
    // is 120 both native and interpreted.
    let dir = unique_temp_dir("local-fn-fact");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    fn fact(n: int): int {\n        if (n <= 1) { return 1 }\n        return n * fact(n - 1)\n    }\n    println(fact(5))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localfact") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "120\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_helper() {
    // Stage 6f: a non-recursive local helper is bound to a closure value and
    // invoked indirectly; dbl(21) is 42.
    let dir = unique_temp_dir("local-fn-dbl");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    fn dbl(x: int): int { return x * 2 }\n    println(dbl(21))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localdbl") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_captures_outer() {
    // Stage 6f: a local function captures an enclosing Copy local through a
    // shared cell (the closure machinery), reading it inside the body; addk(5)
    // with k == 10 is 15.
    let dir = unique_temp_dir("local-fn-capture");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    k = 10\n    fn addk(x: int): int { return x + k }\n    println(addk(5))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localcapk") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "15\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_as_argument() {
    // Stage 6f: a local named function flows as a first-class closure value
    // through a `fn(int): int` parameter and is invoked inside the callee;
    // apply(dbl, 20) is 40.
    let dir = unique_temp_dir("local-fn-arg");
    fs::write(
        dir.join("main.ku"),
        "fn apply(f: fn(int): int, v: int): int { return f(v) }\n\nfn main(): null! {\n    fn dbl(x: int): int { return x * 2 }\n    println(apply(dbl, 20))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localapply") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "40\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_recursive_and_capturing() {
    // Stage 6f soundness: a local function that BOTH captures an outer cell and
    // self-recurses. The self-call threads the running `__env` (holding `base`'s
    // cell) directly instead of re-boxing the function into that env, so there is
    // no reference cycle and the cell is released exactly once. sumdown(3) adds
    // 3 + 2 + 1 + base(100) == 106.
    let dir = unique_temp_dir("local-fn-recap");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    base = 100\n    fn sumdown(n: int): int {\n        if (n <= 0) { return base }\n        return n + sumdown(n - 1)\n    }\n    println(sumdown(3))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localrecap") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "106\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_depth_guard() {
    // Stage 6f: self-recursion respects the shared MAX_CALL_DEPTH guard — a deep
    // local recursion exits cleanly with code 1 ("maximum function call depth
    // exceeded") instead of a native stack-overflow crash, matching the
    // interpreter. A shallow local recursion still runs to completion.
    let dir = unique_temp_dir("local-fn-depth");
    fs::write(
        dir.join("shallow.ku"),
        "fn main(): null! {\n    fn rec(n: int): int {\n        if (n <= 0) { return 0 }\n        return rec(n - 1)\n    }\n    println(rec(5))\n    return ok(null)\n}\n",
    )
    .expect("write shallow.ku");
    fs::write(
        dir.join("deep.ku"),
        "fn main(): null! {\n    fn rec(n: int): int {\n        if (n <= 0) { return 0 }\n        return rec(n - 1)\n    }\n    println(rec(1000))\n    return ok(null)\n}\n",
    )
    .expect("write deep.ku");

    if let Some(exe) = native_build(&dir, "shallow.ku", "localdepthshallow") {
        let (stdout, code) = run_binary(&exe);
        assert_eq!(stdout.replace('\r', ""), "0\n");
        assert_eq!(code, Some(0));
    }
    if let Some(exe) = native_build(&dir, "deep.ku", "localdepthdeep") {
        let (_stdout, code) = run_binary(&exe);
        assert_eq!(code, Some(1), "deep local recursion must exit cleanly");
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_map_matches_interpreter() {
    // Stage 6f: `[T].map(fn(T): U) -> [U]`. The mapper's parameter carries NO
    // annotation, so its type is propagated from the array's element type (the
    // checker infers it, so the interpreter accepts `map(x => x*2)`; native must
    // too — rule 8). The result array is built by invoking the mapper per element.
    let dir = unique_temp_dir("array-map-basic");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    r = [1, 2, 3].map(x => x * 2)\n    println(r[0])\n    println(r[1])\n    println(r[2])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "mapbasic") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "2\n4\n6\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_map_captured_mapper_matches_interpreter() {
    // Stage 6f: the mapper captures an outer cell (`k`). Every element invokes the
    // same env, and the env (and its captured cell) is released exactly once when
    // map finishes — no leak, no double-free (verified under ASan + the CRT debug
    // heap). `[1,2].map(x => x + k)` with k==10 yields 11/12.
    let dir = unique_temp_dir("array-map-capture");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    k = 10\n    r = [1, 2].map(x => x + k)\n    println(r[0])\n    println(r[1])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "mapcapture") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "11\n12\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_map_result_stored_in_variable() {
    // Stage 6f: the map result is a first-class array value — bind it to a local
    // and index it. `[10,20].map(x => x * 3)` yields 30/60.
    let dir = unique_temp_dir("array-map-store");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    d = [10, 20].map(x => x * 3)\n    println(d[0])\n    println(d[1])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "mapstore") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "30\n60\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_void_match_statement_runs_arms() {
    // Regression: a match used as a statement has no value, so lowering must emit
    // the arms as expressions. Storing them into a `void t0` local failed to compile.
    let dir = unique_temp_dir("void-match");
    fs::write(
        dir.join("main.ku"),
        "enum Mode { Hi  Lo }\nfn main(): null! {\n    m = Mode.Hi\n    match m { Mode.Hi => println(\"hi\")  Mode.Lo => println(\"lo\") }\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "voidmatch") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hi\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_owned_local_reassigned_in_loop_does_not_double_free() {
    // Regression: an owned local rebound each iteration must drop its previous
    // value before the new one is stored — and must not drop the value it just
    // took ownership of.
    let dir = unique_temp_dir("loop-owned");
    fs::write(
        dir.join("main.ku"),
        "struct Box { tag: str }\nfn main(): null! {\n    i = 0\n    last = \"\"\n    while (i < 3) {\n        b = Box { tag: \"row\".clone() }\n        s = b.tag\n        last = s\n        i = i + 1\n    }\n    println(last)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "loopowned") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "row\n");
    assert_eq!(code, Some(0), "loop-rebound owned locals must not double-free");

    fs::remove_dir_all(&dir).ok();
}

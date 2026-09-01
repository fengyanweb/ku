//! Checker-level ownership tests for the place-based partial-move analysis.
//! The checker is the first line of defense: it decides which owned reads are
//! moves, tracks moves at struct-field-path granularity, and rejects moving an
//! owned value out of an array/object index (which the C backend cannot
//! move-and-clear). The native backend only executes moves the checker has
//! already accepted, so these rules must hold identically for `check` and `run`.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use ku::cli::{check_source, run_source};

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("ku-owned-{label}-{}-{nonce}", std::process::id()))
}

fn write_same_offset_function_modules(dir: &Path) {
    let relay_prefix = "fn Apply(next: fn(fn(): null): null, op: fn(): null): null {\n";
    let invoke_prefix = "fn Apply(op: fn(): null): null {\n";
    let invoke_leading = " ".repeat(relay_prefix.len() - invoke_prefix.len());
    let invoke_trailing = " ".repeat("next(op)".len() - "op()".len());
    let relay = format!("{relay_prefix}    next(op)\n    return null\n}}\n");
    let invoke =
        format!("{invoke_leading}{invoke_prefix}    op(){invoke_trailing}\n    return null\n}}\n");
    assert_eq!(
        relay.len(),
        invoke.len(),
        "fixture functions must keep matching local body offsets"
    );
    fs::write(dir.join("relay.ku"), relay).expect("write relay module");
    fs::write(dir.join("invoke.ku"), invoke).expect("write invoke module");
}

fn checks(name: &str, source: &str) {
    check_source(name, source)
        .unwrap_or_else(|err| panic!("{name} should check but failed: {}", err.message));
}

fn rejects(name: &str, source: &str, needle: &str) {
    let err = check_source(name, source)
        .err()
        .unwrap_or_else(|| panic!("{name} should have been rejected but checked"));
    assert!(
        err.message.contains(needle),
        "{name}: expected error containing {needle:?}, got: {}",
        err.message
    );
}

// ---- struct field moves are allowed and tracked ------------------------------

#[test]
fn struct_field_move_is_allowed() {
    checks(
        "field-move.ku",
        r#"
struct U { name: str, age: int }
fn main() {
    u = U{ name: "Ku".clone(), age: 1 }
    n = u.name
    println(n)
}
"#,
    );
}

#[test]
fn nested_field_move_is_allowed() {
    checks(
        "nested-move.ku",
        r#"
struct Inner { label: str }
struct Outer { inner: Inner }
fn main() {
    o = Outer{ inner: Inner{ label: "x".clone() } }
    l = o.inner.label
    println(l)
}
"#,
    );
}

#[test]
fn moving_the_same_field_twice_is_rejected() {
    rejects(
        "double-field-move.ku",
        r#"
struct U { name: str }
fn main() {
    u = U{ name: "x".clone() }
    a = u.name
    b = u.name
    println(a)
}
"#,
        "moved",
    );
}

#[test]
fn reading_a_moved_field_is_rejected() {
    rejects(
        "read-moved-field.ku",
        r#"
struct U { name: str }
fn main() {
    u = U{ name: "x".clone() }
    a = u.name
    println(u.name)
}
"#,
        "moved",
    );
}

#[test]
fn sibling_field_stays_usable_after_a_move() {
    checks(
        "sibling.ku",
        r#"
struct U { name: str, tag: str }
fn main() {
    u = U{ name: "n".clone(), tag: "t".clone() }
    n = u.name
    println(u.tag)
}
"#,
    );
}

#[test]
fn nested_sibling_stays_usable_after_a_move() {
    checks(
        "nested-sibling.ku",
        r#"
struct Inner { label: str, tag: str }
struct Outer { inner: Inner }
fn main() {
    o = Outer{ inner: Inner{ label: "l".clone(), tag: "t".clone() } }
    x = o.inner.label
    println(o.inner.tag)
}
"#,
    );
}

#[test]
fn using_the_whole_struct_after_a_partial_move_is_rejected() {
    rejects(
        "whole-after-partial.ku",
        r#"
struct U { name: str, tag: str }
fn take(u: U): str {
    return u.tag.clone()
}
fn main() {
    u = U{ name: "n".clone(), tag: "t".clone() }
    n = u.name
    println(take(u))
}
"#,
        "moved",
    );
}

// ---- re-initialization -------------------------------------------------------

#[test]
fn reassigning_a_moved_field_restores_it() {
    checks(
        "reinit.ku",
        r#"
struct U { name: str }
fn main() {
    u = U{ name: "x".clone() }
    a = u.name
    u.name = "new".clone()
    b = u.name
    println(a)
    println(b)
}
"#,
    );
}

// ---- control flow ------------------------------------------------------------

#[test]
fn a_field_moved_on_one_branch_is_maybe_moved_after() {
    rejects(
        "maybe-moved.ku",
        r#"
struct U { name: str }
fn pick(f: bool) {
    u = U{ name: "x".clone() }
    if (f) {
        a = u.name
    }
    b = u.name
    println(b)
}
fn main() { pick(false) }
"#,
        "may have been moved",
    );
}

#[test]
fn moving_and_reinitializing_on_a_branch_leaves_it_live() {
    checks(
        "branch-reinit.ku",
        r#"
struct U { name: str }
fn pick(f: bool) {
    u = U{ name: "x".clone() }
    if (f) {
        a = u.name
        u.name = "y".clone()
    }
    b = u.name
    println(b)
}
fn main() { pick(true) }
"#,
    );
}

#[test]
fn both_branches_moving_independently_is_allowed() {
    checks(
        "both-branches.ku",
        r#"
struct U { name: str }
fn pick(f: bool): str {
    u = U{ name: "x".clone() }
    if (f) {
        return u.name
    } else {
        return u.name
    }
}
fn main() { println(pick(true)) }
"#,
    );
}

// ---- array / object index require clone --------------------------------------

#[test]
fn moving_an_array_element_requires_clone() {
    rejects(
        "array-move.ku",
        r#"
fn main() {
    xs = ["a".clone(), "b".clone()]
    y = xs[0]
    println(y)
}
"#,
        "clone",
    );
}

#[test]
fn cloning_an_array_element_is_allowed() {
    checks(
        "array-clone.ku",
        r#"
fn main() {
    xs = ["a".clone(), "b".clone()]
    y = xs[0].clone()
    println(y)
    println(xs[1])
}
"#,
    );
}

#[test]
fn reading_an_array_element_read_only_is_allowed() {
    checks(
        "array-read.ku",
        r#"
fn main() {
    xs = ["a".clone(), "b".clone()]
    println(xs[0])
}
"#,
    );
}

// ---- error object fields are struct-backed and movable -----------------------

#[test]
fn error_field_move_is_allowed() {
    checks(
        "error-field.ku",
        r#"
fn boom(): str! {
    fail { domain: "d", code: "c", message: "m" }
}
fn main() {
    m = "none"
    try {
        x = boom()?
        println(x)
    } catch (err) {
        m = err.message
    }
    println(m)
}
"#,
    );
}

// ---- copy types never move ---------------------------------------------------

#[test]
fn copy_field_read_does_not_move() {
    checks(
        "copy-field.ku",
        r#"
struct U { name: str, age: int }
fn main() {
    u = U{ name: "n".clone(), age: 7 }
    a = u.age
    b = u.age
    println(a + b)
}
"#,
    );
}

// ---- adversarial-review regressions ------------------------------------------

#[test]
fn guard_clause_diverging_branch_does_not_poison_fallthrough() {
    // A value moved in a branch that then `return`s never falls through, so the
    // read after the `if` is legal (the move cannot reach it).
    checks(
        "guard-clause.ku",
        r#"
struct User { name: str, age: int }
fn take(s: str) { println(s) }
fn main() {
    user = User { name: "Ku".clone(), age: 1 }
    flag = true
    if (flag) {
        take(user.name)
        return
    }
    println(user.name)
}
"#,
    );
}

#[test]
fn whole_move_in_diverging_else_does_not_poison_fallthrough() {
    checks(
        "diverging-else.ku",
        r#"
fn take(s: str) { println(s) }
fn main() {
    s = "hello".clone()
    flag = true
    if (flag) {
        println("noop")
    } else {
        take(s)
        return
    }
    println(s)
}
"#,
    );
}

#[test]
fn compound_assign_on_a_moved_field_is_rejected() {
    // `+=` reads the target before writing it, so it is a use-after-move.
    rejects(
        "compound-moved.ku",
        r#"
struct User { name: str, age: int }
fn main() {
    u = User { name: "hello".clone(), age: 1 }
    stolen = u.name
    u.name += "WORLD"
    println(stolen)
}
"#,
        "moved",
    );
}

#[test]
fn reading_a_user_object_field_clones_and_leaves_the_source_usable() {
    // A user object literal is a KuObject hash map, not a KuError struct. Reading an
    // entry goes through `ku_object_get_result`, which `ku_value_clone`s it, so the
    // read yields an independent value and moves nothing — the interpreter agrees
    // (it prints the field twice). Requiring `.clone()` here would be a false
    // positive; the array case below is the one that genuinely aliases.
    checks(
        "user-object-field-read.ku",
        r#"
fn main() {
    obj = {domain: "d".clone(), code: "c".clone(), message: "m".clone()}
    taken = obj.domain
    println(taken)
    println(obj.code)
}
"#,
    );
}

#[test]
fn moving_an_array_element_still_requires_clone() {
    // `ku_array_get_str` returns a shallow copy that keeps aliasing the container's
    // buffer, so consuming one double-frees. This is the case the index rule exists
    // for, and it stayed rejected when object reads were reclassified.
    rejects(
        "array-element-move.ku",
        r#"
fn main() {
    xs = ["a".clone()]
    taken = xs[0]
    println(taken)
}
"#,
        "clone",
    );
}

#[test]
fn catch_reading_a_value_moved_then_reinitialized_in_try_is_rejected() {
    // The throw can happen right after the move and before the re-init, so in the
    // catch the value is (maybe) moved, not live.
    rejects(
        "try-throw-point.ku",
        r#"
fn taker(x: str): str! {
    fail "boom"
}
fn main() {
    s = "ORIGINAL".clone()
    try {
        t = taker(s)?
        s = "REINIT".clone()
    } catch (err) {
        println(s)
    }
    println(s)
}
"#,
        "moved",
    );
}

// ---- second adversarial review: control-flow precision -----------------------

#[test]
fn move_then_reinit_in_a_non_throwing_try_is_usable_after() {
    // The try body cannot throw (no `?`/`fail`), so the catch is dead code and the
    // value flow is linear: `s` is moved then re-initialized, and the read after
    // the try/catch is valid.
    checks(
        "try-no-throw.ku",
        r#"
fn take(s: str) {}
fn main() {
    s = "hello".clone()
    try {
        take(s)
        s = "reinit".clone()
    } catch (e) {
        println("caught")
    }
    println(s)
}
"#,
    );
}

#[test]
fn catch_reading_value_moved_before_a_real_throw_is_rejected() {
    // The try body CAN throw (`?`), so the throw can land before the re-init: the
    // catch sees `s` as (maybe) moved.
    rejects(
        "try-throw.ku",
        r#"
fn taker(x: str): str! {
    fail "boom"
}
fn main() {
    s = "ORIG".clone()
    try {
        t = taker(s)?
        s = "REINIT".clone()
    } catch (err) {
        println(s)
    }
    println(s)
}
"#,
        "moved",
    );
}

#[test]
fn finally_reading_value_moved_on_the_throw_path_is_rejected() {
    // `finally` runs even when the body throws before the re-init, so it must see
    // the moved value.
    rejects(
        "finally-throw.ku",
        r#"
fn boom(): str! { fail "e" }
fn take(s: str): int { return 1 }
fn work(): int! {
    s = "ORIGINAL".clone()
    a = 0
    try {
        a = take(s)
        r = boom()?
        s = "REINIT".clone()
        println(r)
    } finally {
        a = take(s)
    }
    return ok(a)
}
fn main(): null! {
    x = work()?
    println(x)
    return ok(null)
}
"#,
        "moved",
    );
}

#[test]
fn assignment_rhs_cannot_move_its_dynamic_object_destination() {
    rejects(
        "assignment-moves-object-target.ku",
        r#"
fn main() {
    obj = { name: "Ku" }
    obj["self"] = obj
}
"#,
        "moved",
    );
}

#[test]
fn assignment_rhs_cannot_move_its_later_index_key() {
    rejects(
        "assignment-moves-index-key.ku",
        r#"
fn main() {
    obj = { name: "Ku" }
    key = "name" + ""
    obj[key] = key
}
"#,
        "moved",
    );
}

#[test]
fn finally_rejects_owned_value_moved_by_a_conditional_return() {
    // The returning branch is intentionally absent from the ordinary `if`
    // fallthrough join, but it still executes finally before leaving the function.
    rejects(
        "finally-conditional-return.ku",
        r#"
fn work(flag: bool): str {
    s = "owned".clone()
    try {
        if (flag) {
            return s
        }
    } finally {
        println(s)
    }
    return s
}
fn main() { println(work(false)) }
"#,
        "moved",
    );
}

#[test]
fn catch_rejects_owned_value_moved_before_a_conditional_fail() {
    rejects(
        "catch-conditional-fail.ku",
        r#"
fn take(s: str) {}
fn work(flag: bool) {
    s = "owned".clone()
    try {
        if (flag) {
            take(s)
            fail "boom"
        }
    } catch (err) {
        println(s)
    }
}
fn main() { work(true) }
"#,
        "moved",
    );
}

#[test]
fn finally_rejects_owned_value_moved_before_a_conditional_fail() {
    rejects(
        "finally-conditional-fail.ku",
        r#"
fn take(s: str) {}
fn work(flag: bool) {
    s = "owned".clone()
    try {
        if (flag) {
            take(s)
            fail "boom"
        }
    } finally {
        println(s)
    }
}
fn main() { work(true) }
"#,
        "moved",
    );
}

#[test]
fn try_assignment_question_does_not_reinitialize_on_error() {
    // `may(s)` consumes `s`; the failing edge of `?` exists before the assignment
    // store can make `s` live again.
    rejects(
        "try-assignment-question.ku",
        r#"
fn may(s: str): str! { return ok(s) }
fn main() {
    s = "owned".clone()
    try {
        s = may(s)?
    } catch (err) {
        println(s)
    }
}
"#,
        "moved",
    );
}

#[test]
fn catch_reinitialization_makes_value_available_to_finally() {
    // On success the assignment stores a new value; on error the catch does. The
    // raw pre-catch throw snapshot must not be merged into finally afterwards.
    checks(
        "catch-reinit-before-finally.ku",
        r#"
fn may(s: str): str! { return ok(s) }
fn main() {
    s = "owned".clone()
    try {
        s = may(s)?
    } catch (err) {
        s = "fallback".clone()
    } finally {
        println(s)
    }
    println(s)
}
"#,
    );
}

#[test]
fn conditional_return_move_does_not_poison_try_fallthrough() {
    // The return path is validated by finally, but cannot reach the println after
    // the try. Keeping the two joins separate preserves ordinary if fallthrough.
    checks(
        "try-return-does-not-poison-fallthrough.ku",
        r#"
fn take(s: str) {}
fn work(flag: bool): null {
    s = "owned".clone()
    try {
        if (flag) {
            take(s)
            return null
        }
    } finally {
        println("cleanup")
    }
    println(s)
    return null
}
fn main() { work(false) }
"#,
    );
}

#[test]
fn while_true_guard_branch_does_not_poison_fallthrough() {
    // A branch ending in `while (true) {}` never falls through, so the move inside
    // it cannot reach the read after the `if`.
    checks(
        "while-true-guard.ku",
        r#"
struct User { name: str, city: str }
fn take(s: str) {}
fn main() {
    u = User { name: "Ku".clone(), city: "HZ".clone() }
    bad = false
    if (bad) {
        take(u.name)
        while (true) { }
    }
    println(u.name)
}
"#,
    );
}

#[test]
fn loop_reinitializing_at_the_top_then_moving_is_allowed() {
    // Each iteration re-establishes `s` before moving it, so it is never reused
    // while moved.
    checks(
        "loop-reinit.ku",
        r#"
fn make(): str { return "x".clone() }
fn take(s: str) { println(s) }
fn main() {
    s = "init".clone()
    xs = [1, 2, 3]
    for x in xs {
        s = make()
        take(s)
    }
    println("done")
}
"#,
    );
}

#[test]
fn loop_carried_move_is_still_rejected() {
    // Moving a value each iteration without re-initializing it reuses a moved value
    // on the next iteration.
    rejects(
        "loop-carried.ku",
        r#"
struct U { name: str }
fn take(s: str) {}
fn main() {
    u = U { name: "x".clone() }
    xs = [1, 2, 3]
    for x in xs {
        take(u.name)
    }
}
"#,
        "moved",
    );
}

// ---- third adversarial review: try/loop dataflow -----------------------------

#[test]
fn a_question_inside_a_destructuring_assignment_makes_the_catch_live() {
    // The `?` in the destructuring can throw, so the catch is reachable; a value
    // the catch moves is moved after the try.
    rejects(
        "destructure-throw.ku",
        r#"
struct U { name: str }
fn sink(u: U): int { println(u.name)  return 0 }
fn mayfail(): int! { fail "boom" }
fn main() {
    x = U{ name: "hi".clone() }
    r = 0
    try {
        a, b = mayfail()?, 0
        println("no throw")
    } catch (e) {
        r = sink(x)
    }
    r = sink(x)
    println("done")
}
"#,
        "moved",
    );
}

#[test]
fn loop_move_then_reinit_in_the_same_iteration_is_allowed() {
    // Each iteration moves `x` and then re-initializes it, so the next iteration's
    // read sees the re-initialized value.
    checks(
        "loop-move-reinit.ku",
        r#"
struct U { name: str }
fn sink(u: U): int { println(u.name)  return 0 }
fn fresh(): U { return U{ name: "z".clone() } }
fn main() {
    x = U{ name: "hi".clone() }
    i = 0
    while (i < 3) {
        r = sink(x)
        x = fresh()
        i = i + 1
    }
    println("done")
}
"#,
    );
}

#[test]
fn value_moved_on_a_break_path_is_moved_after_the_loop() {
    rejects(
        "break-move.ku",
        r#"
struct U { name: str }
fn sink(u: U) { println(u.name) }
fn main() {
    x = U{ name: "hi".clone() }
    while (true) {
        if (true) {
            sink(x)
            break
        }
    }
    sink(x)
}
"#,
        "moved",
    );
}

// ---- fourth adversarial review -----------------------------------------------

#[test]
fn re_matching_an_enum_after_its_owned_payload_was_bound_is_rejected() {
    // Binding an owned enum payload moves it out; a second match reads an emptied
    // payload, so it must be rejected.
    rejects(
        "re-match.ku",
        r#"
enum E { V(x: str)  N }
fn main() {
    e = E.V("hello".clone())
    match e { E.V(x) => println(x)  N => println("n") }
    match e { E.V(y) => println(y)  N => println("n2") }
}
"#,
        "moved",
    );
}

#[test]
fn matching_a_tag_only_enum_does_not_consume_it() {
    // No owned payload is bound, so the enum stays usable.
    checks(
        "tag-enum-rematch.ku",
        r#"
enum Color { Red  Green }
fn main() {
    c = Color.Red
    match c { Color.Red => println("r")  Color.Green => println("g") }
    match c { Color.Red => println("r2")  Color.Green => println("g2") }
}
"#,
    );
}

#[test]
fn cloning_a_partially_moved_struct_is_rejected() {
    // The whole value is read to clone it, but a field was moved out.
    rejects(
        "clone-partial.ku",
        r#"
struct User { name: str, email: str }
fn main() {
    u = User { name: "alice".clone(), email: "e".clone() }
    n = u.name
    v = u.clone()
    println(n)
    println(v.name)
}
"#,
        "moved",
    );
}

// ---- fifth adversarial review -------------------------------------------------

#[test]
fn builtin_arguments_that_take_ownership_record_the_move() {
    // `ok(x)` wraps x into the Result, and the backend moves-and-clears it. The
    // checker used to type-check builtin arguments without consuming them, so this
    // passed and the native binary printed an empty first line while the
    // interpreter printed the value.
    rejects(
        "builtin-arg-move.ku",
        r#"
fn f(): str! {
    s = "he" + "llo"
    r = ok(s)
    println(s)
    return r
}
fn main() { println("start") }
"#,
        "moved",
    );
}

#[test]
fn moving_an_array_element_into_a_builtin_argument_is_rejected() {
    // `ok(xs[0])` reached the backend, which handed the Result a shallow copy that
    // still aliased the array's buffer: the native binary died with
    // STATUS_HEAP_CORRUPTION after the array was dropped.
    rejects(
        "builtin-arg-index.ku",
        r#"
fn f(): str! {
    h: str = "al" + "pha"
    xs: [str] = [h]
    r = ok(xs[0])
    return r
}
fn main() { m = f() }
"#,
        "clone",
    );
}

#[test]
fn read_only_builtin_arguments_are_not_consumed() {
    checks(
        "builtin-arg-borrow.ku",
        r#"
fn main() {
    s = "a"
    println(s)
    println(s)
    xs = ["a"]
    n = len(xs)
    println(n)
    println(xs[0])
}
"#,
    );
}

#[test]
fn a_break_earlier_in_the_body_does_not_hide_a_loop_carried_move() {
    // The loop-top scan ran outside a loop context, so `break` failed as "outside
    // loop" and ended the scan — every move after it became invisible and the
    // loop-carried move below was accepted.
    rejects(
        "loop-break-blindness.ku",
        r#"
fn main() {
    a = "hello"
    i = 0
    while (i < 3) {
        i = i + 1
        if (i > 100) { break }
        b = a
        println(b)
    }
}
"#,
        "moved",
    );
}

#[test]
fn a_move_on_a_continue_path_reaches_the_next_iteration() {
    // `continue` jumps to the top of the iteration, so its moves are loop-carried.
    // `merge_if_scopes` drops the branch as diverging, so the state is recorded at
    // the `continue` itself instead.
    rejects(
        "loop-continue-backedge.ku",
        r#"
fn main() {
    s = "x"
    i = 0
    while (i < 3) {
        i = i + 1
        if (i == 1) {
            t = s
            println(t)
            continue
        }
        println("n")
    }
}
"#,
        "moved",
    );
}

#[test]
fn consuming_a_value_then_breaking_out_of_the_loop_is_allowed() {
    checks(
        "loop-move-then-break.ku",
        r#"
fn main() {
    s = "x"
    i = 0
    while (i < 3) {
        t = s
        println(t)
        break
    }
}
"#,
    );
}

#[test]
fn moving_a_closure_captured_value_is_rejected() {
    // A captured local is boxed into a refcounted cell the closure loads from on
    // every call. The backend moves out of `(cell)->value`, so the closure would
    // then read an emptied cell: the interpreter printed "hello!" where native
    // printed "!". Before the backend even had a move for cells it shallow-copied
    // the payload instead, which double-freed on release.
    rejects(
        "closure-captured-move.ku",
        r#"
fn main() {
    s = "he" + "llo"
    f = () => {
        return s + "!"
    }
    t = s
    println(t)
    println(f())
}
"#,
        "closure captured it",
    );
}

#[test]
fn closure_capture_in_a_fallthrough_branch_survives_the_scope_join() {
    // The closure is stored in `f`, so it escapes the then branch and can read `s`
    // after the if. Dropping `captured=true` at the join let native move-clear the
    // shared cell before `f()` loaded it.
    rejects(
        "branch-closure-capture.ku",
        r#"
fn Noop(): null { return null }
fn take(s: str) {}
fn main() {
    s = "owned"
    f: fn(): null = Noop
    flag = true
    if (flag) {
        f = () => {
            println(s)
            return null
        }
    }
    take(s)
    f()
}
"#,
        "closure captured it",
    );
}

#[test]
fn closure_capture_in_a_returning_branch_does_not_poison_fallthrough() {
    // The branch invokes and destroys its local closure before returning. Only the
    // else path reaches `take(s)`, so merge_if_scopes must keep that path live.
    checks(
        "diverging-branch-closure-capture.ku",
        r#"
fn take(s: str) {}
fn work(flag: bool): null {
    s = "owned"
    if (flag) {
        f = () => {
            println(s)
            return null
        }
        f()
        return null
    }
    take(s)
    return null
}
fn main() { work(false) }
"#,
    );
}

#[test]
fn reading_and_cloning_a_closure_captured_value_stays_legal() {
    checks(
        "closure-captured-read.ku",
        r#"
fn main() {
    s = "x"
    f = () => { return s + "!" }
    println(f())
    println(f())
    println(s)
    t = s.clone()
    println(t)
}
"#,
    );
}

#[test]
fn closure_cannot_be_stored_back_into_its_own_captured_cell() {
    rejects(
        "closure-self-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    f = () => {
        f()
        return null
    }
}
"#,
        "would create a reference cycle",
    );
}

#[test]
fn closure_container_back_edges_are_rejected() {
    rejects(
        "closure-object-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    holder = { callback: Noop }
    holder.callback = () => {
        holder.callback()
        return null
    }
}
"#,
        "would create a reference cycle",
    );

    rejects(
        "closure-array-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    callbacks = [Noop]
    callbacks[0] = () => {
        callbacks[0]()
        return null
    }
}
"#,
        "would create a reference cycle",
    );
}

#[test]
fn alias_returned_closure_cycles_are_rejected() {
    rejects(
        "closure-identity-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Identity(op: fn(): null): fn(): null { return op }
fn main() {
    f: fn(): null = Noop
    f = Identity(() => {
        f()
        return null
    })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "closure-branch-return-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Maybe(op: fn(): null, take: bool): fn(): null {
    if (take) { return op }
    return Noop
}
fn main() {
    f: fn(): null = Noop
    f = Maybe(() => {
        f()
        return null
    }, true)
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "closure-array-factory-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Wrap(op: fn(): null): [fn(): null] { return [op] }
fn main() {
    callbacks = [Noop]
    callbacks = Wrap(() => {
        callbacks[0]()
        return null
    })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "closure-branch-selected-factory-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Identity(op: fn(): null): fn(): null { return op }
fn Discard(op: fn(): null): fn(): null { return Noop }
fn main() {
    factory = Discard
    if (true) { factory = Identity }
    f: fn(): null = Noop
    f = factory(() => {
        f()
        return null
    })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "closure-match-return-cycle.ku",
        r#"
enum Pick { Keep(op: fn(): null)  Drop }
fn Noop(): null { return null }
fn Select(choice: Pick): fn(): null {
    return match choice {
        Pick.Keep(op) => op
        Pick.Drop => Noop
    }
}
fn main() {
    f: fn(): null = Noop
    f = Select(Pick.Keep(() => { f()  return null }))
}
"#,
        "E0904 cannot create closure reference cycle",
    );
    rejects(
        "closure-branch-shadow-does-not-erase-param-provenance.ku",
        r#"
fn Noop(): null { return null }
fn Shadow(op: fn(): null, flag: bool): fn(): null {
    if (flag) {
        op: fn(): null = Noop
        op()
    } else {
        op: fn(): null = Noop
        op()
    }
    return op
}
fn main() {
    f: fn(): null = Noop
    f = Shadow(() => { f()  return null }, true)
}
"#,
        "E0904 cannot create closure reference cycle",
    );
}

#[test]
fn local_factories_preserve_captured_environment_provenance() {
    for (name, source) in [
        (
            "local-closure-factory-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    fn Make(): fn(): null {
        return () => { f()  return null }
    }
    f = Make()
}
"#,
        ),
        (
            "local-array-factory-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    callbacks = [Noop]
    fn Make(): [fn(): null] {
        return [() => { callbacks[0]()  return null }]
    }
    callbacks = Make()
}
"#,
        ),
        (
            "local-object-factory-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    holder = { callback: Noop }
    fn Make() {
        return { callback: () => { holder.callback()  return null } }
    }
    holder = Make()
}
"#,
        ),
    ] {
        rejects(name, source, "E0904 cannot create closure reference cycle");
    }
}

#[test]
fn mutually_captured_closure_writebacks_are_rejected() {
    rejects(
        "closure-mutual-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    g: fn(): null = Noop
    f = () => {
        g()
        return null
    }
    g = () => {
        f()
        return null
    }
}
"#,
        "E0904 cannot create closure reference cycle",
    );
}

#[test]
fn alias_returned_container_field_cycles_are_rejected() {
    for (name, source) in [
        (
            "closure-object-alias-cycle.ku",
            r#"
fn Noop(): null { return null }
fn Identity(op: fn(): null): fn(): null { return op }
fn main() {
    holder = { callback: Noop }
    holder.callback = Identity(() => {
        holder.callback()
        return null
    })
}
"#,
        ),
        (
            "closure-array-alias-cycle.ku",
            r#"
fn Noop(): null { return null }
fn Identity(op: fn(): null): fn(): null { return op }
fn main() {
    callbacks = [Noop]
    callbacks[0] = Identity(() => {
        callbacks[0]()
        return null
    })
}
"#,
        ),
    ] {
        rejects(name, source, "E0904 cannot create closure reference cycle");
    }
}

#[test]
fn projected_closure_dependencies_are_preserved_when_selected() {
    for (name, source) in [
        (
            "closure-array-selection-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    callbacks = [() => { f()  return null }]
    f = callbacks[0].clone()
}
"#,
        ),
        (
            "closure-object-selection-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    holder = { callback: () => { f()  return null } }
    f = holder.callback.clone()
}
"#,
        ),
    ] {
        rejects(name, source, "E0904 cannot create closure reference cycle");
    }

    rejects(
        "closure-for-element-selection-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    callbacks = [() => { f()  return null }]
    for callback in callbacks {
        f = callback.clone()
    }
}
"#,
        "E0904 cannot create closure reference cycle",
    );
}

#[test]
fn projected_closure_writes_detect_mutual_cycles_and_whole_reassign_clears_edges() {
    for (name, source) in [
        (
            "closure-object-mutual-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    a = { callback: Noop }
    b = { callback: Noop }
    a.callback = () => { b.callback()  return null }
    b.callback = () => { a.callback()  return null }
}
"#,
        ),
        (
            "closure-array-mutual-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    a = [Noop]
    b = [Noop]
    a[0] = () => { b[0]()  return null }
    b[0] = () => { a[0]()  return null }
}
"#,
        ),
    ] {
        rejects(name, source, "E0904 cannot create closure reference cycle");
    }

    checks(
        "closure-container-whole-reassign-clears-edge.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    a = { callback: Noop }
    b = { callback: Noop }
    a.callback = () => { b.callback()  return null }
    a = { callback: Noop }
    b.callback = () => { a.callback()  return null }
    b.callback()
}
"#,
    );
}

#[test]
fn non_function_projections_do_not_inherit_container_closure_dependencies() {
    checks(
        "non-function-projection-closure-provenance.ku",
        r#"
fn main() {
    text = "old"
    holder = {
        callback: () => {
            println(text)
            return null
        },
        text: "new"
    }
    text = holder.text.clone()
    println(text)
}
"#,
    );
}

#[test]
fn destructuring_preserves_closure_provenance_and_exactly_clears_whole_bindings() {
    rejects(
        "parallel-destructure-closure-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Identity(op: fn(): null): fn(): null { return op }
fn main() {
    f: fn(): null = Noop
    f, n = Identity(() => { f()  return null }), 1
    println(n)
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "object-destructure-closure-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Identity(op: fn(): null): fn(): null { return op }
fn main() {
    f: fn(): null = Noop
    source = { callback: Identity(() => { f()  return null }) }
    { callback: f } = source
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "object-rest-destructure-closure-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    rest = { callback: Noop }
    source = { callback: () => { rest.callback()  return null } }
    { ...rest } = source
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    checks(
        "destructure-whole-reassign-clears-closure-edge.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    g: fn(): null = Noop
    f = () => { g()  return null }
    f, n = Noop, 1
    g = () => { f()  return null }
    println(n)
    g()
}
"#,
    );

    checks(
        "non-function-object-destructure-clears-provenance.ku",
        r#"
fn main() {
    text = "old"
    holder = {
        callback: () => { println(text)  return null },
        text: "new"
    }
    { text } = holder
    println(text)
}
"#,
    );
}

#[test]
fn local_function_capture_cycles_are_rejected_but_self_recursion_is_not() {
    rejects(
        "local-function-capture-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    fn Capture(): null {
        f()
        return null
    }
    f = Capture
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    checks(
        "local-function-self-recursion.ku",
        r#"
fn main() {
    fn Down(value: int): int {
        if (value <= 0) { return 0 }
        return Down(value - 1)
    }
    down = Down.clone()
    println(down(2))
}
"#,
    );
}

#[test]
fn captured_setter_calls_cannot_install_a_back_referencing_closure() {
    rejects(
        "local-function-setter-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    fn Set(value: fn(): null): null {
        f = value
        return null
    }
    Set(() => { f()  return null })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "closure-setter-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    setter: fn(fn(): null): null = (value: fn(): null) => {
        f = value
        return null
    }
    setter(() => { f()  return null })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    checks(
        "captured-setter-safe-value.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    fn Set(value: fn(): null): null {
        f = value
        return null
    }
    Set(Noop)
    f()
}
"#,
    );
}

#[test]
fn nested_and_projected_setter_effects_cannot_hide_closure_cycles() {
    rejects(
        "nested-setter-effect-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    fn Set(value: fn(): null): null {
        f = value
        return null
    }
    fn Wrap(value: fn(): null): null {
        Set(value)
        return null
    }
    Wrap(() => { f()  return null })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    for (name, source) in [
        (
            "array-projected-setter-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    setters = [(value: fn(): null) => { f = value  return null }]
    setters[0](() => { f()  return null })
}
"#,
        ),
        (
            "object-projected-setter-cycle.ku",
            r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    holder = {
        setter: (value: fn(): null) => { f = value  return null }
    }
    holder.setter(() => { f()  return null })
}
"#,
        ),
    ] {
        rejects(name, source, "E0904 cannot create closure reference cycle");
    }
}

#[test]
fn top_level_higher_order_wrappers_instantiate_concrete_setter_effects() {
    rejects(
        "top-level-wrapper-setter-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Install(setter: fn(fn(): null): null, value: fn(): null): null {
    return setter(value)
}
fn main() {
    f: fn(): null = Noop
    setter = (value: fn(): null) => {
        f = value
        return null
    }
    Install(setter, () => { f()  return null })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    checks(
        "top-level-wrapper-discard-is-safe.ku",
        r#"
fn Noop(): null { return null }
fn Discard(setter: fn(fn(): null): null, value: fn(): null): null {
    return null
}
fn main() {
    f: fn(): null = Noop
    setter = (value: fn(): null) => {
        f = value
        return null
    }
    Discard(setter, () => { f()  return null })
    f()
}
"#,
    );
}

#[test]
fn nested_and_recursive_top_level_wrappers_preserve_effects_with_bounded_analysis() {
    rejects(
        "nested-top-level-wrapper-setter-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Install(setter: fn(fn(): null): null, value: fn(): null): null {
    return setter(value)
}
fn Relay(setter: fn(fn(): null): null, value: fn(): null): null {
    return Install(setter, value)
}
fn main() {
    f: fn(): null = Noop
    setter = (value: fn(): null) => {
        f = value
        return null
    }
    Relay(setter, () => { f()  return null })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "recursive-top-level-wrapper-setter-cycle.ku",
        r#"
fn Noop(): null { return null }
fn Install(setter: fn(fn(): null): null, value: fn(): null, depth: int): null {
    if (depth > 0) {
        Install(setter, value, depth - 1)
    } else {
        setter(value)
    }
    return null
}
fn main() {
    f: fn(): null = Noop
    setter = (value: fn(): null) => {
        f = value
        return null
    }
    Install(setter, () => { f()  return null }, 2)
}
"#,
        "E0904 cannot create closure reference cycle",
    );
}

#[test]
fn erased_setter_and_outer_capture_identity_are_fail_closed() {
    rejects(
        "erased-escaping-setter-cycle.ku",
        r#"
fn Noop(): null { return null }
fn MakeSetter(): fn(fn(): null): null {
    cell: fn(): null = Noop
    return (value: fn(): null) => {
        cell = value
        return null
    }
}
fn main() {
    setter = MakeSetter()
    setter(() => {
        setter(Noop)
        return null
    })
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    rejects(
        "effect-closure-capture-cell-identity.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    g: fn(): null = Noop
    fn Install(): null {
        g = () => { f()  return null }
        return null
    }
    g = Noop
    Install()
    f = () => { g()  return null }
}
"#,
        "E0904 cannot create closure reference cycle",
    );
}

#[test]
fn loop_closure_provenance_reaches_a_bounded_fixed_point() {
    rejects(
        "loop-multi-iteration-closure-cycle.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    a: fn(): null = Noop
    b: fn(): null = Noop
    c: fn(): null = Noop
    d: fn(): null = Noop
    keepGoing = true
    while (keepGoing) {
        a = b.clone()
        b = c.clone()
        c = d.clone()
        d = () => { a()  return null }
    }
}
"#,
        "E0904 cannot create closure reference cycle",
    );
}

#[test]
fn closure_summary_cache_and_budget_are_bounded_and_fail_closed() {
    let mut cached = String::from(
        "fn Noop(): null { return null }\n\
         fn W0(op: fn(): null, flag: bool): fn(): null {\n\
             if (flag) { return op }\n\
             return op\n\
         }\n",
    );
    for index in 1..=28 {
        cached.push_str(&format!(
            "fn W{index}(op: fn(): null, flag: bool): fn(): null {{\n\
                 if (flag) {{ return W{}(op, flag) }}\n\
                 return W{}(op, flag)\n\
             }}\n",
            index - 1,
            index - 1,
        ));
    }
    cached.push_str(
        "fn main() {\n\
             f: fn(): null = Noop\n\
             f = W28(() => { f()  return null }, true)\n\
         }\n",
    );
    let started = Instant::now();
    rejects(
        "closure-summary-cache.ku",
        &cached,
        "E0904 cannot create closure reference cycle",
    );
    assert!(
        started.elapsed().as_secs_f32() < 5.0,
        "cached closure summary should remain near-linear, took {:?}",
        started.elapsed()
    );

    let mut budgeted = String::from(
        "fn Noop(): null { return null }\n\
         fn B0(op: fn(): null): fn(): null { return op }\n",
    );
    for index in 1..=104 {
        budgeted.push_str(&format!(
            "fn B{index}(op: fn(): null): fn(): null {{ return B{}(op) }}\n",
            index - 1,
        ));
    }
    budgeted.push_str(
        "fn main() {\n\
             f: fn(): null = Noop\n\
             f = B104(() => { f()  return null })\n\
         }\n",
    );
    rejects(
        "closure-summary-budget.ku",
        &budgeted,
        "E0904 cannot create closure reference cycle",
    );
}

#[test]
fn closure_dependency_graph_joins_branches_uses_binding_ids_and_clears_on_reassign() {
    rejects(
        "closure-cycle-after-branch-join.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    g: fn(): null = Noop
    flag = true
    if (flag) {
        f = () => { g()  return null }
    } else {
        f = Noop
    }
    g = () => { f()  return null }
}
"#,
        "E0904 cannot create closure reference cycle",
    );

    checks(
        "closure-shadowed-binding-ids.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    observer = () => { f()  return null }
    if (true) {
        f: fn(): null = Noop
        f = () => { observer()  return null }
        f()
    }
    observer()
}
"#,
    );

    checks(
        "closure-reassign-clears-edge.ku",
        r#"
fn Noop(): null { return null }
fn main() {
    f: fn(): null = Noop
    g: fn(): null = Noop
    f = () => { g()  return null }
    f = Noop
    g = () => { f()  return null }
    g()
}
"#,
    );
}

#[test]
fn closure_may_still_capture_a_different_binding() {
    checks(
        "closure-acyclic-capture.ku",
        r#"
fn Noop(): str { return "" }
fn main() {
    text = "owned"
    f: fn(): str = Noop
    f = () => { return text.clone() }
    println(f())
}
"#,
    );

    checks(
        "closure-temporary-capture.ku",
        r#"
fn Noop(): null { return null }
fn Discard(op: fn(): null): fn(): null { return Noop }
fn main() {
    f: fn(): null = Noop
    f = Discard(() => {
        f()
        return null
    })
    f()
}
"#,
    );

    checks(
        "closure-temporary-capture-through-concrete-alias.ku",
        r#"
fn Noop(): null { return null }
fn Discard(op: fn(): null): fn(): null { return Noop }
fn main() {
    f: fn(): null = Noop
    discard = Discard
    f = discard(() => {
        f()
        return null
    })
    f()
}
"#,
    );
}

#[test]
fn a_move_after_the_last_throwing_statement_does_not_poison_the_catch() {
    // The only statement that can throw runs before `u.name` is moved, so at every
    // reachable throw point the field is still live. The throw-state accumulation
    // used to fold in every move regardless of position and reject the catch body.
    checks(
        "try-move-after-throw.ku",
        r#"
struct U { name: str }
fn boom(): str! { fail "bad" }
fn main(): null! {
    u = U{ name: "A".clone() }
    try {
        v = boom()?
        n = u.name
        println(n)
        println(v)
    } catch (err) {
        println(u.name)
    }
    return ok(null)
}
"#,
    );
}

#[test]
fn a_move_before_a_throwing_statement_still_poisons_the_catch() {
    rejects(
        "try-move-before-throw.ku",
        r#"
struct U { name: str }
fn boom(): str! { fail "bad" }
fn main(): null! {
    u = U{ name: "A".clone() }
    try {
        n = u.name
        println(n)
        v = boom()?
        println(v)
    } catch (err) {
        println(u.name)
    }
    return ok(null)
}
"#,
        "moved",
    );
}

// ---- std.pg pooled prepared-query binding --------------------------------------

#[test]
fn pg_client_and_result_type_check() {
    checks(
        "pg-ok.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({
        conninfo: "host=localhost",
        max_connections: 4,
        max_waiters: 64,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 30000
    })?
    res = client.query("SELECT $1", ["x"])?
    println(res.rows())
    println(res.cols())
    println(res.value(0, 0)?)
    println(res.is_null(0, 0)?)
    client.close()
    return ok(null)
}
"#,
    );
}

#[test]
fn pg_client_defaults_and_empty_params_type_check() {
    checks(
        "pg-pool.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost" })?
    res = client.query("SELECT 1", [])?
    println(res.value(0, 0)?)
    client.close()
    return ok(null)
}
"#,
    );
}

#[test]
fn database_method_receivers_must_be_bound_before_use() {
    for (name, source) in [
        (
            "pg-temporary-client.ku",
            r#"
import pg from "std.pg"
fn main(): null! {
    result = (pg.client({ conninfo: "host=localhost" })?).query("SELECT 1", [])?
    println(result.rows())
    return ok(null)
}
"#,
        ),
        (
            "pg-temporary-result.ku",
            r#"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost" })?
    value = (client.query("SELECT 1", [])?).value(0, 0)?
    println(value)
    return ok(null)
}
"#,
        ),
        (
            "redis-temporary-client.ku",
            r#"
import redis from "std.redis"
fn main(): null! {
    (redis.client({ host: "127.0.0.1" })?).ping()?
    return ok(null)
}
"#,
        ),
        (
            "mysql-temporary-client.ku",
            r#"
import mysql from "std.mysql"
fn main(): null! {
    result = (mysql.client({ host: "127.0.0.1", user: "u", password: "p", database: "db" })?).query("SELECT 1", [])?
    println(result.rows())
    return ok(null)
}
"#,
        ),
        (
            "mysql-temporary-result.ku",
            r#"
import mysql from "std.mysql"
fn main(): null! {
    client = mysql.client({ host: "127.0.0.1", user: "u", password: "p", database: "db" })?
    value = (client.query("SELECT 1", [])?).value(0, 0)?
    println(value)
    return ok(null)
}
"#,
        ),
    ] {
        rejects(name, source, "assigned to a binding");
    }
}

#[test]
fn pg_using_a_closed_client_is_rejected() {
    rejects(
        "pg-client-closed.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost" })?
    client.close()
    res = client.query("SELECT 1", [])?
    println(res.value(0, 0)?)
    return ok(null)
}
"#,
        "moved",
    );
}

#[test]
fn pg_passing_a_result_where_a_client_is_expected_is_rejected() {
    rejects(
        "pg-wrong-handle.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost" })?
    res = client.query("SELECT 1", [])?
    other = res.query("SELECT 2", [])?
    println(other.rows())
    return ok(null)
}
"#,
        "pg_result",
    );
}

// ---- std.mysql pooled prepared-statement binding -------------------------------

#[test]
fn mysql_commands_type_check() {
    checks(
        "mysql-ok.ku",
        r#"
import mysql from "std.mysql"
fn main(): null! {
    client = mysql.client({ host: "127.0.0.1", user: "u", password: "p", database: "db" })?
    res = client.query("SELECT ? AS a", ["x"])?
    println(res.rows())
    println(res.cols())
    println(res.value(0, 0)?)
    println(res.is_null(0, 0)?)
    println(client.execute("UPDATE example SET value = ?", ["x"])?)
    client.close()
    return ok(null)
}
"#,
    );
}

#[test]
fn mysql_using_a_closed_client_is_rejected() {
    rejects(
        "mysql-use-after-close.ku",
        r#"
import mysql from "std.mysql"
fn main(): null! {
    client = mysql.client({ host: "127.0.0.1", user: "u", password: "p", database: "db" })?
    client.close()
    res = client.query("SELECT 1", [])?
    println(res.rows())
    return ok(null)
}
"#,
        "moved",
    );
}

#[test]
fn mysql_legacy_module_apis_are_rejected() {
    for legacy_call in [
        "mysql.connect(\"127.0.0.1\", 3306, \"u\", \"p\", \"db\")?",
        "mysql.query(null, \"SELECT 1\")?",
        "mysql.query_params(null, \"SELECT ?\", [\"x\"])?",
        "mysql.execute(null, \"SELECT 1\", [])?",
        "mysql.rows(null)",
        "mysql.cols(null)",
        "mysql.value(null, 0, 0)?",
        "mysql.is_null(null, 0, 0)?",
        "mysql.close(null)",
    ] {
        rejects(
            "mysql-legacy-api.ku",
            &format!(
                "import mysql from \"std.mysql\"\nfn main(): null! {{\n    {legacy_call}\n    return ok(null)\n}}\n"
            ),
            "unknown stdlib function",
        );
    }
}

#[test]
fn mysql_client_config_shape_is_checked() {
    for (name, body, message) in [
        (
            "mysql-config-missing-host.ku",
            "client = mysql.client({ user: \"u\", password: \"p\", database: \"db\" })?",
            "requires string field 'host'",
        ),
        (
            "mysql-config-unknown-field.ku",
            "client = mysql.client({ host: \"h\", user: \"u\", password: \"p\", database: \"db\", pool_size: 8 })?",
            "unknown mysql client config field 'pool_size'",
        ),
        (
            "mysql-config-wrong-password.ku",
            "client = mysql.client({ host: \"h\", user: \"u\", password: 1, database: \"db\" })?",
            "field 'password' must be str",
        ),
        (
            "mysql-config-wrong-timeout.ku",
            "client = mysql.client({ host: \"h\", user: \"u\", password: \"p\", database: \"db\", query_timeout_ms: \"slow\" })?",
            "field 'query_timeout_ms' must be int",
        ),
    ] {
        rejects(
            name,
            &format!(
                "import mysql from \"std.mysql\"\nfn main(): null! {{\n    {body}\n    return ok(null)\n}}\n"
            ),
            message,
        );
    }
}

// ---- std.redis RESP-over-socket binding ----------------------------------------

#[test]
fn redis_commands_type_check() {
    checks(
        "redis-ok.ku",
        r#"
import redis from "std.redis"
fn main(): null! {
    client = redis.client({ host: "127.0.0.1", port: 6379,
        username: "default", password: "secret", max_connections: 8,
        max_waiters: 64, connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000, command_timeout_ms: 5000 })?
    client.ping()?
    client.set("k", "v")?
    println(client.get("k")?)
    println(client.exists("k")?)
    println(client.del("k")?)
    client.close()
    return ok(null)
}
"#,
    );
}

#[test]
fn pg_legacy_module_apis_are_rejected() {
    for legacy_call in [
        "pg.connect(\"host=localhost\")?",
        "pg.connect_timeout(\"host=localhost\", 5)?",
        "pg.pool(\"host=localhost\", 4)?",
        "pg.query(null, \"SELECT 1\")?",
        "pg.query_params(null, \"SELECT 1\", [])?",
        "pg.pool_query(null, \"SELECT 1\")?",
        "pg.pool_query_params(null, \"SELECT 1\", [])?",
        "pg.close(null)",
        "pg.pool_close(null)",
    ] {
        rejects(
            "pg-legacy-api.ku",
            &format!(
                "import pg from \"std.pg\"\nfn main(): null! {{\n    {legacy_call}\n    return ok(null)\n}}\n"
            ),
            "is not public",
        );
    }
    for internal_call in [
        "pg_client.query(null, \"SELECT 1\", [])?",
        "pg_result.rows(null)",
    ] {
        rejects(
            "pg-private-api.ku",
            &format!("fn main(): null! {{\n    {internal_call}\n    return ok(null)\n}}\n"),
            "is not public",
        );
    }
}

#[test]
fn pg_client_config_and_query_shape_are_checked() {
    for (name, body, message) in [
        (
            "pg-config-missing-conninfo.ku",
            "client = pg.client({ max_connections: 1 })?",
            "requires string field 'conninfo'",
        ),
        (
            "pg-config-unknown-field.ku",
            "client = pg.client({ conninfo: \"host=localhost\", maximum: 1 })?",
            "unknown pg client config field 'maximum'",
        ),
        (
            "pg-config-wrong-conninfo.ku",
            "client = pg.client({ conninfo: 1 })?",
            "field 'conninfo' must be str",
        ),
        (
            "pg-config-wrong-limit.ku",
            "client = pg.client({ conninfo: \"host=localhost\", max_connections: \"8\" })?",
            "field 'max_connections' must be int",
        ),
        (
            "pg-query-missing-params.ku",
            "client = pg.client({ conninfo: \"host=localhost\" })?\n    result = client.query(\"SELECT 1\")?",
            "pass [] when there are no parameters",
        ),
    ] {
        rejects(
            name,
            &format!(
                "import pg from \"std.pg\"\nfn main(): null! {{\n    {body}\n    return ok(null)\n}}\n"
            ),
            message,
        );
    }
}

#[test]
fn redis_old_module_and_raw_connection_apis_are_rejected() {
    for (name, source, message) in [
        (
            "redis-old-connect.ku",
            r#"
import redis from "std.redis"
fn main(): null! {
    client = redis.connect("127.0.0.1", 6379)?
    return ok(null)
}
"#,
            "unknown stdlib function 'redis.connect'",
        ),
        (
            "redis-old-module-command.ku",
            r#"
import redis from "std.redis"
fn main(): null! {
    client = redis.client({ host: "127.0.0.1" })?
    value = redis.get(client, "key")?
    println(value)
    return ok(null)
}
"#,
            "unknown stdlib function 'redis.get'",
        ),
    ] {
        rejects(name, source, message);
    }
    for (function, arguments, suffix) in [
        ("auth", "null, \"secret\"", "?"),
        ("ping", "null", "?"),
        ("get_required", "null, \"key\"", "?"),
        ("set", "null, \"key\", \"value\"", "?"),
        ("del", "null, \"key\"", "?"),
        ("exists", "null, \"key\"", "?"),
        ("close", "null", ""),
    ] {
        rejects(
            "redis-removed-module-api.ku",
            &format!(
                "import redis from \"std.redis\"\nfn main(): null! {{\n    redis.{function}({arguments}){suffix}\n    return ok(null)\n}}\n"
            ),
            &format!("unknown stdlib function 'redis.{function}'"),
        );
    }
}

#[test]
fn redis_static_client_config_is_strict() {
    for (name, config, message) in [
        (
            "redis-config-unknown.ku",
            "{ host: \"127.0.0.1\", pool_size: 8 }",
            "unknown redis client config field 'pool_size'",
        ),
        (
            "redis-config-missing-host.ku",
            "{ max_connections: 8 }",
            "requires string field 'host'",
        ),
        (
            "redis-config-wrong-type.ku",
            "{ host: \"127.0.0.1\", max_connections: \"8\" }",
            "config field 'max_connections' must be int",
        ),
        (
            "redis-config-username-without-password.ku",
            "{ host: \"127.0.0.1\", username: \"alice\" }",
            "field 'username' requires 'password'",
        ),
    ] {
        rejects(
            name,
            &format!(
                "import redis from \"std.redis\"\nfn main(): null! {{\n    client = redis.client({config})?\n    client.close()\n    return ok(null)\n}}\n"
            ),
            message,
        );
    }
}

#[test]
fn redis_using_a_closed_client_is_rejected() {
    rejects(
        "redis-use-after-close.ku",
        r#"
import redis from "std.redis"
fn main(): null! {
    client = redis.client({ host: "127.0.0.1" })?
    client.close()
    println(client.get("k")?)
    return ok(null)
}
"#,
        "moved",
    );
}

#[test]
fn native_resource_handles_cannot_be_cloned() {
    for (name, source) in [
        (
            "redis-clone.ku",
            r#"
import redis from "std.redis"
fn main(): null! {
    client = redis.client({ host: "127.0.0.1" })?
    duplicate = client.clone()
    duplicate.close()
    return ok(null)
}
"#,
        ),
        (
            "pg-client-clone.ku",
            r#"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost", max_connections: 2 })?
    duplicate = client.clone()
    duplicate.close()
    return ok(null)
}
"#,
        ),
        (
            "pg-client-result-clone.ku",
            r#"
import pg from "std.pg"
fn main() {
    created = pg.client({ conninfo: "host=localhost" })
    duplicate = created.clone()
    println(duplicate)
}
"#,
        ),
        (
            "pg-handle-array-clone.ku",
            r#"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost" })?
    handles = [client]
    duplicate = handles.clone()
    println(duplicate)
    return ok(null)
}
"#,
        ),
        (
            "redis-handle-object-clone.ku",
            r#"
import redis from "std.redis"
fn main(): null! {
    client = redis.client({ host: "127.0.0.1" })?
    wrapper = { client: client }
    duplicate = wrapper.clone()
    println(duplicate)
    return ok(null)
}
"#,
        ),
    ] {
        rejects(name, source, "native resource handles cannot be cloned");
    }
}

#[test]
fn readonly_http_handler_cannot_consume_outer_native_handles() {
    for (name, source, moved_name) in [
        (
            "http-handler-close-pg-client.ku",
            r#"
import "std.http"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost", max_connections: 1 })?
    app = http.service()
    app.get("/close", fn() {
        client.close()
        return http.text("closed")
    })
    return ok(null)
}
"#,
            "client",
        ),
        (
            "http-handler-close-redis.ku",
            r#"
import "std.http"
import redis from "std.redis"
fn main(): null! {
    cache = redis.client({ host: "127.0.0.1" })?
    app = http.service()
    app.get("/close", fn() {
        cache.close()
        return http.text("closed")
    })
    return ok(null)
}
"#,
            "cache",
        ),
        (
            "http-handler-close-mysql-client.ku",
            r#"
import "std.http"
import mysql from "std.mysql"
fn main(): null! {
    client = mysql.client({
        host: "127.0.0.1", user: "user", password: "password", database: "db",
        max_connections: 1
    })?
    app = http.service()
    app.get("/close", fn() {
        client.close()
        return http.text("closed")
    })
    return ok(null)
}
"#,
            "client",
        ),
    ] {
        rejects(
            name,
            source,
            &format!("http handler cannot move captured owned value '{moved_name}'"),
        );
    }
}

#[test]
fn http_handler_rejects_concurrent_unsafe_database_handle_captures() {
    for (name, source, native_name) in [
        (
            "http-handler-pg-result.ku",
            r#"
import "std.http"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost" })?
    result = client.query("SELECT 1", [])?
    app = http.service()
    app.get("/", fn() {
        pending = result.rows()
        return http.text("ok")
    })
    return ok(null)
}
"#,
            "pg_result",
        ),
        (
            "http-handler-mysql-result.ku",
            r#"
import "std.http"
import mysql from "std.mysql"
fn main(): null! {
    db = mysql.client({ host: "localhost", user: "user", password: "password", database: "db" })?
    result = db.query("SELECT 1", [])?
    app = http.service()
    app.get("/", fn() {
        pending = result.value(0, 0)
        return http.text("ok")
    })
    return ok(null)
}
"#,
            "mysql_result",
        ),
    ] {
        rejects(
            name,
            source,
            &format!("cannot share captured native resource '{native_name}'"),
        );
    }
}

#[test]
fn http_handler_allows_thread_compatible_native_captures() {
    checks(
        "http-handler-thread-compatible-native.ku",
        r#"
import "std.http"
import pg from "std.pg"
import redis from "std.redis"
import mysql from "std.mysql"
fn main(): null! {
    pg_db = pg.client({ conninfo: "host=localhost", max_connections: 2 })?
    cache = redis.client({ host: "127.0.0.1" })?
    mysql_db = mysql.client({ host: "localhost", user: "user", password: "password", database: "db" })?
    app = http.service()
    app.get("/pg", fn() {
        pending = pg_db.query("SELECT 1", [])
        return http.text("ok")
    })
    app.get("/redis", fn() {
        pending = cache.get("key")
        return http.text("ok")
    })
    app.get("/mysql", fn() {
        pending = mysql_db.query("SELECT 1", [])
        return http.text("ok")
    })
    return ok(null)
}
"#,
    );
}

#[test]
fn http_handler_reaudits_captured_function_values_and_rejects_mutation() {
    rejects(
        "http-handler-captured-mutator.ku",
        r#"
import "std.http"
fn main(): null! {
    count = 0
    mutate = () => {
        count += 1
        return null
    }
    app = http.service()
    app.get("/", fn() {
        mutate()
        return http.text("ok")
    })
    return ok(null)
}
"#,
        "http handler cannot modify captured variable 'count'",
    );

    // A function annotation carries only its signature. If a factory erases the
    // concrete body, the handler audit must fail closed instead of assuming it is
    // read-only.
    rejects(
        "http-handler-erased-function-body.ku",
        r#"
import "std.http"
fn Noop(): null { return null }
fn Erase(op: fn(): null): fn(): null { return op }
fn main(): null! {
    handler = Erase(Noop)
    app = http.service()
    app.get("/", handler)
    return ok(null)
}
"#,
        "cannot prove a function value is read-only because its body is unavailable",
    );
}

#[test]
fn http_handler_allows_readonly_captured_function_value() {
    checks(
        "http-handler-readonly-helper.ku",
        r#"
import "std.http"
fn main(): null! {
    render = () => { return http.text("ok") }
    app = http.service()
    app.get("/", fn() { return render() })
    return ok(null)
}
"#,
    );
}

#[test]
fn http_handler_local_closure_may_mutate_handler_local_state() {
    checks(
        "http-handler-local-closure-mutation.ku",
        r#"
import "std.http"
fn main(): null! {
    app = http.service()
    app.get("/", fn() {
        count = 0
        bump = () => {
            count += 1
            return null
        }
        bump()
        return http.text("ok")
    })
    return ok(null)
}
"#,
    );
}

#[test]
fn http_handler_readonly_function_audit_terminates_on_recursion() {
    checks(
        "http-handler-recursive-helper.ku",
        r#"
import "std.http"
fn main(): null! {
    app = http.service()
    app.get("/", fn() {
        fn down(value: int): int {
            if (value <= 0) { return 0 }
            return down(value - 1)
        }
        println(down(2))
        return http.text("ok")
    })
    return ok(null)
}
"#,
    );

    checks(
        "http-handler-mutually-recursive-helpers.ku",
        r#"
import "std.http"
fn Even(value: int): int {
    if (value <= 0) { return 1 }
    return Odd(value - 1)
}
fn Odd(value: int): int {
    if (value <= 0) { return 0 }
    return Even(value - 1)
}
fn main(): null! {
    classify = Even
    app = http.service()
    app.get("/", fn() {
        println(classify(2))
        return http.text("ok")
    })
    return ok(null)
}
"#,
    );
}

#[test]
fn http_handler_imported_same_offset_bodies_do_not_hide_captured_mutation() {
    let dir = unique_temp_dir("readonly-import-mutation");
    fs::create_dir_all(&dir).expect("create import collision directory");
    write_same_offset_function_modules(&dir);
    let main_path = dir.join("main.ku");
    let source = r#"
import "std.http"
import { Apply as Relay } from "./relay.ku"
import { Apply as Invoke } from "./invoke.ku"
fn main(): null! {
    count = 0
    mutate = () => {
        count += 1
        return null
    }
    relay = Relay
    invoke = Invoke
    app = http.service()
    app.get("/", fn() {
        relay(invoke.clone(), mutate.clone())
        return http.text("ok")
    })
    return ok(null)
}
"#;
    fs::write(&main_path, source).expect("write import collision entry");
    rejects(
        &main_path.to_string_lossy(),
        source,
        "http handler cannot modify captured variable 'count'",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn http_handler_imported_same_offset_bodies_do_not_hide_native_capture() {
    let dir = unique_temp_dir("readonly-import-native");
    fs::create_dir_all(&dir).expect("create native import collision directory");
    write_same_offset_function_modules(&dir);
    let main_path = dir.join("main.ku");
    let source = r#"
import "std.http"
import pg from "std.pg"
import { Apply as Relay } from "./relay.ku"
import { Apply as Invoke } from "./invoke.ku"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost" })?
    result = client.query("SELECT 1", [])?
    query = () => {
        pending = result.rows()
        return null
    }
    relay = Relay
    invoke = Invoke
    app = http.service()
    app.get("/", fn() {
        relay(invoke.clone(), query.clone())
        return http.text("ok")
    })
    return ok(null)
}
"#;
    fs::write(&main_path, source).expect("write native import collision entry");
    rejects(
        &main_path.to_string_lossy(),
        source,
        "cannot share captured native resource 'pg_result'",
    );
    fs::remove_dir_all(&dir).ok();
}

// ---- reserved compiler namespace ----------------------------------------------

#[test]
fn user_names_in_the_compiler_reserved_prefix_are_rejected() {
    // The native C backend emits block-scoped temporaries under `__ku_`. A user
    // binding of the same name was silently shadowed in the generated C: `__ku_p`
    // printed an empty string and `__ku_store` segfaulted, both with a clean build.
    for (file, source) in [
        (
            "reserved-local.ku",
            "fn main() {\n    __ku_p = \"v\".clone()\n    println(__ku_p)\n}\n",
        ),
        (
            "reserved-fn.ku",
            "fn __ku_go(): int { return 1 }\nfn main() { println(__ku_go()) }\n",
        ),
        (
            "reserved-param.ku",
            "fn f(__ku_a: int): int { return __ku_a }\nfn main() { println(f(1)) }\n",
        ),
        (
            "reserved-field.ku",
            "struct S { __ku_x: int }\nfn main() {\n    s = S { __ku_x: 1 }\n    println(s.__ku_x)\n}\n",
        ),
    ] {
        rejects(file, source, "reserved");
    }
}

#[test]
fn ordinary_names_are_unaffected_by_the_reserved_prefix() {
    checks(
        "unreserved.ku",
        "fn ku_go(): int { return 1 }\nfn main() {\n    ku_p = ku_go()\n    println(ku_p)\n}\n",
    );
}

// ---- runtime parity: a partial move actually runs -----------------------------

#[test]
fn partial_move_runs_on_the_interpreter() {
    let source = r#"
struct U { name: str, tag: str }
fn main() {
    u = U{ name: "kept".clone(), tag: "also".clone() }
    n = u.name
    println(n)
    println(u.tag)
}
"#;
    checks("partial-run.ku", source);
    run_source("partial-run.ku", source).expect("partial move should run");
}

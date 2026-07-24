//! Checker-level ownership tests for the place-based partial-move analysis.
//! The checker is the first line of defense: it decides which owned reads are
//! moves, tracks moves at struct-field-path granularity, and rejects moving an
//! owned value out of an array/object index (which the C backend cannot
//! move-and-clear). The native backend only executes moves the checker has
//! already accepted, so these rules must hold identically for `check` and `run`.

use ku::cli::{check_source, run_source};

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

// ---- std.pg thin libpq binding -------------------------------------------------

#[test]
fn pg_connection_and_result_type_check() {
    checks(
        "pg-ok.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    conn = pg.connect("host=localhost")?
    res = pg.query(conn, "SELECT 1")?
    println(pg.rows(res))
    println(pg.value(res, 0, 0))
    pg.close(conn)
    return ok(null)
}
"#,
    );
}

#[test]
fn pg_connection_pool_type_checks() {
    checks(
        "pg-pool.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    pool = pg.pool("host=localhost", 4)?
    res = pg.pool_query(pool, "SELECT 1")?
    println(pg.value(res, 0, 0))
    res2 = pg.pool_query_params(pool, "SELECT $1", ["x"])?
    println(pg.value(res2, 0, 0))
    pg.pool_close(pool)
    return ok(null)
}
"#,
    );
}

#[test]
fn pg_using_a_closed_pool_is_rejected() {
    rejects(
        "pg-pool-closed.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    pool = pg.pool("host=localhost", 4)?
    pg.pool_close(pool)
    res = pg.pool_query(pool, "SELECT 1")?
    println(pg.value(res, 0, 0))
    return ok(null)
}
"#,
        "moved",
    );
}

#[test]
fn pg_using_a_closed_connection_is_rejected() {
    // pg.close consumes the connection; a later use must be a moved-value error.
    rejects(
        "pg-use-after-close.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    conn = pg.connect("host=localhost")?
    pg.close(conn)
    res = pg.query(conn, "SELECT 1")?
    println(pg.rows(res))
    return ok(null)
}
"#,
        "moved",
    );
}

#[test]
fn pg_passing_a_result_where_a_connection_is_expected_is_rejected() {
    rejects(
        "pg-wrong-handle.ku",
        r#"
import pg from "std.pg"
fn main(): null! {
    conn = pg.connect("host=localhost")?
    res = pg.query(conn, "SELECT 1")?
    other = pg.query(res, "SELECT 2")?
    println(pg.rows(other))
    return ok(null)
}
"#,
        "pg_result",
    );
}

// ---- std.mysql thin libmysqlclient binding -------------------------------------

#[test]
fn mysql_commands_type_check() {
    checks(
        "mysql-ok.ku",
        r#"
import mysql from "std.mysql"
fn main(): null! {
    conn = mysql.connect("127.0.0.1", 3306, "u", "p", "db")?
    res = mysql.query_params(conn, "SELECT ? AS a", ["x"])?
    println(mysql.rows(res))
    println(mysql.value(res, 0, 0))
    mysql.close(conn)
    return ok(null)
}
"#,
    );
}

#[test]
fn mysql_using_a_closed_connection_is_rejected() {
    rejects(
        "mysql-use-after-close.ku",
        r#"
import mysql from "std.mysql"
fn main(): null! {
    conn = mysql.connect("127.0.0.1", 3306, "u", "p", "db")?
    mysql.close(conn)
    res = mysql.query(conn, "SELECT 1")?
    println(mysql.rows(res))
    return ok(null)
}
"#,
        "moved",
    );
}

// ---- std.redis RESP-over-socket binding ----------------------------------------

#[test]
fn redis_commands_type_check() {
    checks(
        "redis-ok.ku",
        r#"
import redis from "std.redis"
fn main(): null! {
    conn = redis.connect("127.0.0.1", 6379)?
    redis.auth(conn, "secret")?
    redis.set(conn, "k", "v")?
    println(redis.get(conn, "k")?)
    println(redis.del(conn, "k")?)
    redis.close(conn)
    return ok(null)
}
"#,
    );
}

#[test]
fn redis_using_a_closed_connection_is_rejected() {
    rejects(
        "redis-use-after-close.ku",
        r#"
import redis from "std.redis"
fn main(): null! {
    conn = redis.connect("127.0.0.1", 6379)?
    redis.close(conn)
    println(redis.get(conn, "k")?)
    return ok(null)
}
"#,
        "moved",
    );
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

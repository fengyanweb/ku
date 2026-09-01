use ku::cli::{check_source, run_source};

fn check_and_run(source: &str) {
    check_source("collection-regression.ku", source).expect("collection program should check");
    run_source("collection-regression.ku", source).expect("collection program should run");
}

#[test]
fn interpreter_collection_reads_preserve_receiver_before_effectful_index() {
    check_and_run(
        r#"
fn main() {
    values = [10, 20]
    choose = () => {
        values = [70, 80]
        return 1
    }
    selected = values[choose()]
    if (selected != 20 || values[1] != 80) panic("index evaluation order changed")
    index = 0
    if (values[index + 1] != 80 || values.len() != 2) panic("plain projection changed")
    characters = "A界😀".chars()
    if (characters.len() != 3 || characters[index + 1] != "界") panic("Unicode projection changed")
    empty:[int] = []
    if (!empty.is_empty() || empty.len() != 0) panic("empty array projection changed")
}
"#,
    );
}

#[test]
fn interpreter_collection_self_push_preserves_owned_copies_and_pure_push() {
    check_and_run(
        r#"
fn main() {
    values:[int] = []
    for index in 4096 {
        values = values.push(index)
    }
    if (values.len() != 4096 || values[4095] != 4095) panic("self push lost values")
    more = values.push(4096)
    if (values.len() != 4096 || more.len() != 4097) panic("ordinary push mutated its input")

    row = ["owned" + " string"]
    rows:[[str]] = []
    rows = rows.push(row)
    rows = rows.push(row)
    row[0] = "changed"
    rows[0][0] = "first changed"
    if (rows[1][0] != "owned string" || row[0] != "changed") panic("push did not retain deep copies")
}
"#,
    );
}

#[test]
fn interpreter_collection_self_push_keeps_short_circuit_arguments() {
    check_and_run(
        r#"
fn main() {
    values = [true]
    values = values.push(false && (1 / 0 == 0))
    if (values.len() != 2 || values[1] != false) panic("append evaluated short-circuited RHS")
}
"#,
    );
}

#[test]
fn interpreter_collection_self_push_keeps_captured_and_effectful_argument_semantics() {
    check_and_run(
        r#"
fn main() {
    values = [1]
    make_piece = () => {
        values = [9]
        return 2
    }
    values = values.push(make_piece())
    if (values.len() != 2 || values[0] != 1 || values[1] != 2) panic("push receiver order changed")
    observe = () => { return values.len() }
    piece = 3
    values = values.push(piece)
    if (observe() != 3 || values[2] != 3) panic("captured binding became stale")
}
"#,
    );
}

#[test]
fn interpreter_collection_string_compound_append_preserves_rhs_first_and_failure() {
    check_and_run(
        r#"
fn Missing(): str! {
    fail "missing"
}
fn main(): null! {
    text = "A"
    for index in 4096 {
        text += "x"
    }
    if (text.len() != 4097) panic("append lost content")
    text = "界"
    text += text.clone()
    if (text != "界界") panic("self clone append changed")
    right = () => {
        text = "changed"
        return "!"
    }
    text += right()
    if (text != "changed!") panic("compound assignment must evaluate RHS first")
    caught = false
    finalized = false
    try {
        text += Missing()?
    } catch (err) {
        caught = true
    } finally {
        finalized = true
    }
    if (!caught || !finalized || text != "changed!") panic("failed RHS changed string")
    return ok(null)
}
"#,
    );
}

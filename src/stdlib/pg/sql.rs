use super::{Error, PgResult};

fn keyword_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn next_keyword<'a>(sql: &mut &'a [u8], check: &impl Fn() -> PgResult<()>) -> PgResult<&'a [u8]> {
    let bytes = *sql;
    let mut at = 0;
    let mut next_check = 0;
    let mut advance_check = |at| -> PgResult<()> {
        if at >= next_check {
            check()?;
            next_check = at + 4096;
        }
        Ok(())
    };
    loop {
        advance_check(at)?;
        match bytes.get(at..) {
            Some([b' ' | b'\t' | b'\r' | b'\n' | b'\x0b' | b'\x0c', ..]) => at += 1,
            Some([b'-', b'-', ..]) => {
                at += 2;
                while let Some(byte) = bytes.get(at) {
                    advance_check(at)?;
                    if matches!(byte, b'\r' | b'\n') {
                        break;
                    }
                    at += 1;
                }
            }
            Some([b'/', b'*', ..]) => {
                at += 2;
                let mut depth = 1;
                while depth != 0 {
                    advance_check(at)?;
                    match bytes.get(at..) {
                        Some([b'/', b'*', ..]) => {
                            depth += 1;
                            at += 2;
                        }
                        Some([b'*', b'/', ..]) => {
                            depth -= 1;
                            at += 2;
                        }
                        Some([_, ..]) => at += 1,
                        _ => return Err(SESSION_CONTROL),
                    }
                }
            }
            Some([b';', ..]) => return Err(SESSION_CONTROL),
            _ => break,
        }
    }
    let start = at;
    if bytes
        .get(at)
        .is_some_and(|byte| keyword_byte(*byte) && !byte.is_ascii_digit())
    {
        while bytes.get(at).is_some_and(|byte| keyword_byte(*byte)) {
            advance_check(at)?;
            at += 1;
        }
    }
    *sql = &bytes[at..];
    Ok(&bytes[start..at])
}

const SESSION_CONTROL: Error = super::error(
    "session_state_unsupported",
    "PostgreSQL statement was not sent because explicit transaction or session-control SQL is unsupported by the pooled client",
);

// libpq permits one statement per parameterized send. Match native PG's
// leading-token check; hidden session effects still require idle-state and reset checks.
pub(super) fn validate(sql: &str, check: &impl Fn() -> PgResult<()>) -> PgResult<()> {
    let mut sql = sql.strip_prefix('\u{feff}').unwrap_or(sql).as_bytes();
    let first = next_keyword(&mut sql, check)?;
    let forbidden = [
        "begin",
        "start",
        "commit",
        "end",
        "rollback",
        "abort",
        "savepoint",
        "release",
        "set",
        "reset",
        "discard",
        "declare",
        "fetch",
        "move",
        "close",
        "copy",
        "listen",
        "unlisten",
        "notify",
        "lock",
        "prepare",
        "execute",
        "deallocate",
        "load",
    ];
    if forbidden
        .iter()
        .any(|word| first.eq_ignore_ascii_case(word.as_bytes()))
    {
        return Err(SESSION_CONTROL);
    }
    let create = first.eq_ignore_ascii_case(b"create");
    if create || first.eq_ignore_ascii_case(b"drop") {
        let mut keyword = next_keyword(&mut sql, check)?;
        if create && keyword.eq_ignore_ascii_case(b"or") {
            if !next_keyword(&mut sql, check)?.eq_ignore_ascii_case(b"replace") {
                return Ok(());
            }
            keyword = next_keyword(&mut sql, check)?;
        }
        if create
            && (keyword.eq_ignore_ascii_case(b"global") || keyword.eq_ignore_ascii_case(b"local"))
        {
            keyword = next_keyword(&mut sql, check)?;
        }
        if keyword.eq_ignore_ascii_case(b"temp") || keyword.eq_ignore_ascii_case(b"temporary") {
            return Err(SESSION_CONTROL);
        }
    }
    Ok(())
}

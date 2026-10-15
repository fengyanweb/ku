use std::collections::HashMap;

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::{
        core::{expect_arg_count, expected_type},
        errors,
    },
    value::Value,
};

const MAX_JSON_INPUT_BYTES: usize = 1_000_000;
const MAX_JSON_DEPTH: usize = 32;
const MAX_JSON_OUTPUT_BYTES: usize = 1_000_000;

mod yyjson;

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "parse" | "try_parse" | "decode" => {
            expect_arg_count(&format!("json.{function}"), args.len(), 1, span)?;
            let Value::String(text) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            Ok(Some(parse_result(text, span)))
        }
        "stringify" | "encode" => {
            expect_arg_count(&format!("json.{function}"), args.len(), 1, span)?;
            let result = match stringify_value(&args[0], span) {
                Ok(text) => errors::ok(Value::String(text)),
                Err(err) => errors::err(
                    err.domain.as_deref().unwrap_or("json"),
                    err.code.as_deref().unwrap_or("stringify_error"),
                    err.message,
                ),
            };
            Ok(Some(result))
        }
        _ => Ok(None),
    }
}

fn parse_result(text: &str, span: Span) -> Value {
    match parse_json(text, span) {
        Ok(value) => errors::ok(value),
        Err(err) => errors::err(
            err.domain.as_deref().unwrap_or("json"),
            err.code.as_deref().unwrap_or("parse_error"),
            err.message,
        ),
    }
}

fn allocation_error(span: Span) -> KuError {
    let mut error = KuError::runtime("out of memory converting JSON", span);
    error.domain = Some("json".into());
    error.code = Some("out_of_memory".into());
    error
}

pub fn stringify_value(value: &Value, span: Span) -> KuResult<String> {
    let builder = yyjson::Builder::new().ok_or_else(|| allocation_error(span))?;
    // Charge canonical output bytes before DOM copies/allocations, including
    // Ku-compatible float and escape spelling. No unbounded DOM is built first.
    let mut remaining = MAX_JSON_OUTPUT_BYTES;
    let root = encode_value(&builder, value, 0, &mut remaining, span)?;
    let text = builder.write(root).map_err(|failure| {
        if failure.code == 2 {
            allocation_error(span)
        } else {
            KuError::runtime(failure.message, span)
        }
    })?;
    canonical_text(text.bytes(), span)
}

fn charge(remaining: &mut usize, bytes: usize, span: Span) -> KuResult<()> {
    *remaining = remaining
        .checked_sub(bytes)
        .ok_or_else(|| KuError::runtime("json.stringify output is too large", span))?;
    Ok(())
}

fn charge_string(remaining: &mut usize, text: &str, prepaid: usize, span: Span) -> KuResult<()> {
    charge(remaining, 2 - prepaid, span)?;
    for byte in text.bytes() {
        let encoded = match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
            0..=0x1f => 6,
            _ => 1,
        };
        charge(remaining, encoded, span)?;
    }
    Ok(())
}

fn encode_value<'a>(
    builder: &'a yyjson::Builder,
    value: &Value,
    depth: usize,
    remaining: &mut usize,
    span: Span,
) -> KuResult<yyjson::Mutable<'a>> {
    if depth > MAX_JSON_DEPTH {
        return Err(KuError::runtime("json value nesting is too deep", span));
    }
    if let Value::Borrowed(view) = value {
        return view.with_read(span, |value| {
            encode_value(builder, value, depth, remaining, span)
        });
    }
    charge(remaining, 1, span)?;
    let node = match value {
        Value::Null => {
            charge(remaining, 3, span)?;
            builder.null()
        }
        Value::Bool(value) => {
            charge(remaining, if *value { 3 } else { 4 }, span)?;
            builder.boolean(*value)
        }
        Value::Int(value) => {
            charge(remaining, value.to_string().len() - 1, span)?;
            builder.integer(*value)
        }
        Value::Float(value) => {
            if !value.is_finite() {
                return Err(KuError::runtime(
                    "json.stringify does not support non-finite float",
                    span,
                ));
            }
            // Preserve Ku's shortest fixed-decimal representation. This trusted
            // numeric token is written by yyjson without converting via double.
            let number = value.to_string();
            charge(remaining, number.len() - 1, span)?;
            builder.number(&number)
        }
        Value::String(value) => {
            charge_string(remaining, value, 1, span)?;
            builder.string(value)
        }
        Value::Array(values) => {
            charge(remaining, 1 + values.len().saturating_sub(1), span)?;
            if values.len() > *remaining {
                return Err(KuError::runtime("json.stringify output is too large", span));
            }
            let array = builder.array().ok_or_else(|| allocation_error(span))?;
            for value in values {
                let child = encode_value(builder, value, depth + 1, remaining, span)?;
                if !builder.append(&array, child) {
                    return Err(allocation_error(span));
                }
            }
            Some(array)
        }
        Value::Object(fields) | Value::Struct { fields, .. } => {
            charge(remaining, 1 + fields.len().saturating_sub(1), span)?;
            let object = builder.object().ok_or_else(|| allocation_error(span))?;
            // Bound temporary sort storage before allocating it. Values are
            // borrowed throughout; sorting never changes the source HashMap.
            for key in fields.keys() {
                charge_string(remaining, key, 0, span)?;
                charge(remaining, 1, span)?;
            }
            let mut sorted = Vec::new();
            sorted
                .try_reserve_exact(fields.len())
                .map_err(|_| allocation_error(span))?;
            sorted.extend(fields.iter());
            sorted.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for (key, value) in sorted {
                let key = builder.string(key).ok_or_else(|| allocation_error(span))?;
                let child = encode_value(builder, value, depth + 1, remaining, span)?;
                if !builder.add(&object, key, child) {
                    return Err(allocation_error(span));
                }
            }
            Some(object)
        }
        other => {
            return Err(KuError::runtime(
                format!("json.stringify does not support {}", other.type_name()),
                span,
            ))
        }
    };
    node.ok_or_else(|| allocation_error(span))
}

/// yyjson's compact escapes are equivalent JSON, but existing Ku output uses
/// \u0008/\u000c. Skip escape pairs so a literal backslash-b is never rewritten.
fn canonical_text(bytes: &[u8], span: Span) -> KuResult<String> {
    let mut length = bytes.len();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            if matches!(bytes[index + 1], b'b' | b'f') {
                length = length
                    .checked_add(4)
                    .ok_or_else(|| KuError::runtime("json.stringify output is too large", span))?;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    if length > MAX_JSON_OUTPUT_BYTES {
        return Err(KuError::runtime("json.stringify output is too large", span));
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| allocation_error(span))?;
    index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            match bytes[index + 1] {
                b'b' => output.extend_from_slice(b"\\u0008"),
                b'f' => output.extend_from_slice(b"\\u000c"),
                _ => output.extend_from_slice(&bytes[index..index + 2]),
            }
            index += 2;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| KuError::runtime("invalid JSON UTF-8 output", span))
}

fn parse_json(text: &str, span: Span) -> KuResult<Value> {
    if text.len() > MAX_JSON_INPUT_BYTES {
        return Err(KuError::runtime("json input is too large", span));
    }
    let doc = yyjson::Document::read(text).map_err(|failure| {
        if failure.code == 2 {
            return allocation_error(span);
        }
        let number = text.trim_start_matches([' ', '\t', '\r', '\n']).as_bytes();
        let unsigned = number.strip_prefix(b"-").unwrap_or(number);
        let numeric = number
            .first()
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-');
        let leading_zero =
            unsigned.first() == Some(&b'0') && unsigned.get(1).is_some_and(u8::is_ascii_digit);
        let message = if leading_zero && matches!(failure.code, 4 | 9) {
            "leading zero in json number"
        } else if numeric && matches!(failure.code, 5 | 9) {
            "expected digit in json number"
        } else {
            match failure.code {
                4 => "unexpected trailing characters",
                15 => "json nesting is too deep",
                _ => &failure.message,
            }
        };
        KuError::runtime(
            format!("json parse failed: {message} at byte {}", failure.position),
            span,
        )
    })?;
    decode_value(doc.root(), 0, span)
}

fn owned_text(text: &str, span: Span) -> KuResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|_| allocation_error(span))?;
    owned.push_str(text);
    Ok(owned)
}

fn decode_value(node: yyjson::Node<'_>, depth: usize, span: Span) -> KuResult<Value> {
    if depth > MAX_JSON_DEPTH {
        return Err(KuError::runtime("json nesting is too deep", span));
    }
    Ok(match node.kind() {
        0 => Value::Null,
        1 => Value::Bool(node.boolean()),
        2 => Value::Int(node.integer()),
        3 => Value::Int(i64::try_from(node.unsigned()).map_err(|_| {
            KuError::runtime("invalid json number: integer is outside int64 range", span)
        })?),
        4 => Value::Float(node.real()),
        5 => Value::String(owned_text(node.text(), span)?),
        6 => {
            let mut values = Vec::new();
            values
                .try_reserve_exact(node.len())
                .map_err(|_| allocation_error(span))?;
            for child in node.array_items() {
                values.push(decode_value(child, depth + 1, span)?);
            }
            Value::Array(values)
        }
        7 => {
            let mut fields = HashMap::new();
            for (key, value) in node.object_fields() {
                fields.try_reserve(1).map_err(|_| allocation_error(span))?;
                fields.insert(
                    owned_text(key.text(), span)?,
                    decode_value(value, depth + 1, span)?,
                );
            }
            Value::Object(fields)
        }
        _ => {
            let raw = node.raw_number();
            let message = if raw.contains(['.', 'e', 'E']) {
                "json number must be finite"
            } else {
                "invalid json number: integer is outside int64 range"
            };
            return Err(KuError::runtime(message, span));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yyjson_deep_input_is_rejected_before_building_a_ku_tree() {
        let text = format!("{}0{}", "[".repeat(4096), "]".repeat(4096));
        let error = parse_json(&text, Span::default()).expect_err("bounded reader depth");
        assert!(error.message.contains("json nesting is too deep"));
        let value = parse_json("{\"after\":[\"中\\u0000😀\",7]}", Span::default()).unwrap();
        // The document was already freed by parse_json. Retained Ku data is
        // independently owned and can be read/encoded after another read fails.
        assert!(parse_json("[false,}", Span::default()).is_err());
        assert_eq!(
            stringify_value(&value, Span::default()).unwrap(),
            "{\"after\":[\"中\\u0000😀\",7]}"
        );
    }

    #[test]
    fn yyjson_nodes_cannot_be_attached_to_another_document() {
        let first = yyjson::Builder::new().unwrap();
        let second = yyjson::Builder::new().unwrap();
        let array = first.array().unwrap();
        assert!(!first.append(&array, second.string("foreign").unwrap()));
        assert!(first.write(second.null().unwrap()).is_err());
        assert!(first.append(&array, first.integer(7).unwrap()));
        assert_eq!(first.write(array).unwrap().bytes(), b"[7]");
    }

    #[test]
    fn stringify_rejects_every_non_finite_float() {
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let error = stringify_value(&Value::Float(value), Span::default())
                .expect_err("non-finite floats must not produce invalid JSON");
            assert!(
                error.message.contains("non-finite float"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn strings_accept_json_legal_del_and_c1_controls() {
        for character in ['\u{007f}', '\u{0085}'] {
            let text = format!("\"{character}\"");
            let parsed = parse_json(&text, Span::default())
                .expect("JSON permits unescaped code points above U+001F");
            assert_eq!(parsed, Value::String(character.to_string()));
            assert_eq!(
                stringify_value(&parsed, Span::default()).expect("stringify legal control"),
                text
            );
        }

        let error = parse_json("\"\u{001f}\"", Span::default())
            .expect_err("JSON forbids unescaped U+0000 through U+001F");
        assert!(
            error.message.contains("control character"),
            "unexpected error: {error}"
        );
    }
}

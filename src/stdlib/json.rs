use std::collections::HashMap;

use crate::{
    error::{KuError, KuResult},
    span::{Position, Span},
    stdlib::{
        core::{expect_arg_count, expected_type},
        errors,
    },
    value::Value,
};

const MAX_JSON_INPUT_BYTES: usize = 1_000_000;
const MAX_JSON_DEPTH: usize = 32;
const MAX_JSON_OUTPUT_BYTES: usize = 1_000_000;

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "parse" => {
            expect_arg_count("json.parse", args.len(), 1, span)?;
            let Value::String(text) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            Ok(Some(parse_result(text, span)))
        }
        "try_parse" => {
            expect_arg_count("json.try_parse", args.len(), 1, span)?;
            let Value::String(text) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            Ok(Some(parse_result(text, span)))
        }
        "stringify" => {
            expect_arg_count("json.stringify", args.len(), 1, span)?;
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

pub fn stringify_value(value: &Value, span: Span) -> KuResult<String> {
    let mut output = String::new();
    write_json(value, &mut output, 0, span)?;
    if output.len() > MAX_JSON_OUTPUT_BYTES {
        return Err(KuError::runtime("json.stringify output is too large", span));
    }
    Ok(output)
}

fn parse_json(text: &str, span: Span) -> KuResult<Value> {
    if text.len() > MAX_JSON_INPUT_BYTES {
        return Err(KuError::runtime("json input is too large", span));
    }
    let mut parser = JsonParser::new(text, span);
    let value = parser.value(0)?;
    parser.skip_ws();
    if !parser.is_done() {
        return Err(parser.error("unexpected trailing characters"));
    }
    Ok(value)
}

struct JsonParser<'a> {
    chars: Vec<char>,
    current: usize,
    span: Span,
    _source: &'a str,
}

impl<'a> JsonParser<'a> {
    fn new(source: &'a str, span: Span) -> Self {
        Self {
            chars: source.chars().collect(),
            current: 0,
            span,
            _source: source,
        }
    }

    fn value(&mut self, depth: usize) -> KuResult<Value> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.error("json nesting is too deep"));
        }
        self.skip_ws();
        match self.peek() {
            Some('"') => self.string().map(Value::String),
            Some('n') => {
                self.literal("null")?;
                Ok(Value::Null)
            }
            Some('t') => {
                self.literal("true")?;
                Ok(Value::Bool(true))
            }
            Some('f') => {
                self.literal("false")?;
                Ok(Value::Bool(false))
            }
            Some('[') => self.array(depth + 1),
            Some('{') => self.object(depth + 1),
            Some('-' | '0'..='9') => self.number(),
            Some(_) => Err(self.error("expected json value")),
            None => Err(self.error("unexpected end of json")),
        }
    }

    fn array(&mut self, depth: usize) -> KuResult<Value> {
        self.consume('[')?;
        let mut values = Vec::new();
        self.skip_ws();
        if self.matches(']') {
            return Ok(Value::Array(values));
        }
        loop {
            values.push(self.value(depth)?);
            self.skip_ws();
            if self.matches(']') {
                break;
            }
            self.consume(',')?;
        }
        Ok(Value::Array(values))
    }

    fn object(&mut self, depth: usize) -> KuResult<Value> {
        self.consume('{')?;
        let mut fields = HashMap::new();
        self.skip_ws();
        if self.matches('}') {
            return Ok(Value::Object(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.consume(':')?;
            let value = self.value(depth)?;
            fields.insert(key, value);
            self.skip_ws();
            if self.matches('}') {
                break;
            }
            self.consume(',')?;
        }
        Ok(Value::Object(fields))
    }

    fn string(&mut self) -> KuResult<String> {
        self.consume('"')?;
        let mut output = String::new();
        while let Some(ch) = self.advance() {
            match ch {
                '"' => return Ok(output),
                '\\' => {
                    let Some(escaped) = self.advance() else {
                        return Err(self.error("unterminated json escape"));
                    };
                    match escaped {
                        '"' => output.push('"'),
                        '\\' => output.push('\\'),
                        '/' => output.push('/'),
                        'b' => output.push('\u{0008}'),
                        'f' => output.push('\u{000c}'),
                        'n' => output.push('\n'),
                        'r' => output.push('\r'),
                        't' => output.push('\t'),
                        'u' => output.push(self.unicode_escape()?),
                        _ => return Err(self.error("invalid json escape")),
                    }
                }
                ch if (ch as u32) < 0x20 => {
                    return Err(self.error("control character in json string"));
                }
                _ => output.push(ch),
            }
        }
        Err(self.error("unterminated json string"))
    }

    fn unicode_escape(&mut self) -> KuResult<char> {
        let high = self.unicode_code_unit()?;
        let scalar = if (0xD800..=0xDBFF).contains(&high) {
            if self.advance() != Some('\\') || self.advance() != Some('u') {
                return Err(self.error("high surrogate must be followed by a low surrogate"));
            }
            let low = self.unicode_code_unit()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(self.error("high surrogate must be followed by a low surrogate"));
            }
            0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
        } else if (0xDC00..=0xDFFF).contains(&high) {
            return Err(self.error("unexpected low surrogate"));
        } else {
            high
        };
        char::from_u32(scalar).ok_or_else(|| self.error("invalid unicode scalar"))
    }

    fn unicode_code_unit(&mut self) -> KuResult<u32> {
        let mut value = 0u32;
        for _ in 0..4 {
            let Some(ch) = self.advance() else {
                return Err(self.error("unterminated unicode escape"));
            };
            let Some(digit) = ch.to_digit(16) else {
                return Err(self.error("invalid unicode escape"));
            };
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn number(&mut self) -> KuResult<Value> {
        let start = self.current;
        self.matches('-');
        self.consume_integer_digits()?;
        let mut is_float = false;
        if self.matches('.') {
            is_float = true;
            if self.consume_digits() == 0 {
                return Err(self.error("expected digit after decimal point"));
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.advance();
            let _ = self.matches('+') || self.matches('-');
            if self.consume_digits() == 0 {
                return Err(self.error("expected digit in exponent"));
            }
        }
        let text = self.chars[start..self.current].iter().collect::<String>();
        if is_float {
            let value = text
                .parse::<f64>()
                .map_err(|_| self.error("invalid json number"))?;
            if !value.is_finite() {
                return Err(self.error("json number must be finite"));
            }
            Ok(Value::Float(value))
        } else {
            text.parse::<i64>()
                .map(Value::Int)
                .map_err(|_| self.error("invalid json number"))
        }
    }

    fn consume_integer_digits(&mut self) -> KuResult<()> {
        if self.matches('0') {
            if matches!(self.peek(), Some('0'..='9')) {
                return Err(self.error("leading zero in json number"));
            }
            return Ok(());
        }
        if self.consume_digits() == 0 {
            return Err(self.error("expected digit in json number"));
        }
        Ok(())
    }

    fn consume_digits(&mut self) -> usize {
        let start = self.current;
        while matches!(self.peek(), Some('0'..='9')) {
            self.advance();
        }
        self.current - start
    }

    fn literal(&mut self, literal: &str) -> KuResult<()> {
        for expected in literal.chars() {
            self.consume(expected)?;
        }
        Ok(())
    }

    fn consume(&mut self, expected: char) -> KuResult<()> {
        if self.matches(expected) {
            Ok(())
        } else {
            Err(self.error(format!("expected '{expected}'")))
        }
    }

    fn matches(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.current += 1;
        Some(ch)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.current).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\r' | '\n' | '\t')) {
            self.advance();
        }
    }

    fn is_done(&self) -> bool {
        self.current >= self.chars.len()
    }

    fn error(&self, message: impl Into<String>) -> KuError {
        let position = Position::new(self.span.start.line, self.span.start.column, self.current);
        KuError::runtime(message, Span::point(position))
    }
}

fn write_json(value: &Value, output: &mut String, depth: usize, span: Span) -> KuResult<()> {
    if depth > MAX_JSON_DEPTH {
        return Err(KuError::runtime("json value nesting is too deep", span));
    }
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Int(value) => output.push_str(&value.to_string()),
        Value::Float(value) => {
            if !value.is_finite() {
                return Err(KuError::runtime(
                    "json.stringify does not support non-finite float",
                    span,
                ));
            }
            output.push_str(&value.to_string());
        }
        Value::String(value) => write_json_string(value, output),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_json(value, output, depth + 1, span)?;
            }
            output.push(']');
        }
        Value::Object(fields) | Value::Struct { fields, .. } => {
            output.push('{');
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in fields.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_json_string(key, output);
                output.push(':');
                write_json(value, output, depth + 1, span)?;
            }
            output.push('}');
        }
        Value::Enum { .. }
        | Value::Result { .. }
        | Value::Function { .. }
        | Value::Task(_)
        | Value::HttpListenerLease(_) => {
            return Err(KuError::runtime(
                format!("json.stringify does not support {}", value.type_name()),
                span,
            ));
        }
    }
    if output.len() > MAX_JSON_OUTPUT_BYTES {
        return Err(KuError::runtime("json.stringify output is too large", span));
    }
    Ok(())
}

fn write_json_string(value: &str, output: &mut String) {
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if (ch as u32) < 0x20 => output.push_str(&format!("\\u{:04x}", ch as u32)),
            _ => output.push(ch),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

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

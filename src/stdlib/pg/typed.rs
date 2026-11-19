use crate::{span::Span, value::Value};

use super::{
    check_for_nul, error, Error, PgResult, INPUT_CHUNK, MAX_PARAM_BYTES, MAX_PARAM_COUNT,
    OUT_OF_MEMORY,
};

pub(super) const BOOL: u32 = 16;
pub(super) const INT8: u32 = 20;
pub(super) const INT4: u32 = 23;
pub(super) const TEXT: u32 = 25;
pub(super) const VARCHAR: u32 = 1043;
const MAX_COLUMNS: usize = 4096;

pub(super) const SCHEMA_ERROR: Error = error(
    "typed_value_error",
    "PostgreSQL typed declaration or parameter shape is invalid",
);
pub(super) const OID_ERROR: Error = error(
    "typed_value_error",
    "PostgreSQL result OID differs from the declared type",
);
pub(super) const FORMAT_ERROR: Error = error(
    "typed_value_error",
    "PostgreSQL typed result requires binary format",
);
pub(super) const LENGTH_ERROR: Error = error(
    "typed_value_error",
    "PostgreSQL binary value has an invalid length",
);
pub(super) const NULL_ERROR: Error = error(
    "null_value",
    "PostgreSQL NULL requires a nullable type declaration",
);
pub(super) const ROWS_ERROR: Error = error(
    "row_count_error",
    "PostgreSQL typed query requires exactly one row; the statement is not retried",
);
pub(super) const TYPE_ERROR: Error = error(
    "typed_value_error",
    "PostgreSQL value does not match its declared type",
);

#[derive(Clone, Copy)]
pub(super) struct Column {
    pub(super) oid: u32,
    pub(super) nullable: bool,
}

impl Column {
    fn parse(value: &Value, span: Span) -> PgResult<Self> {
        read(value, span, |value| {
            let Value::String(name) = value else {
                return Err(SCHEMA_ERROR);
            };
            let nullable = name.ends_with('?');
            let oid = match name.strip_suffix('?').unwrap_or(name) {
                "int4" => INT4,
                "int8" => INT8,
                "bool" => BOOL,
                "text" => TEXT,
                "varchar" => VARCHAR,
                _ => return Err(SCHEMA_ERROR),
            };
            Ok(Self { oid, nullable })
        })
    }

    pub(super) fn is_text(self) -> bool {
        matches!(self.oid, TEXT | VARCHAR)
    }
}

pub(super) enum Cell {
    Null,
    Int(i64),
    Bool(bool),
    Text(String),
}

enum Input<'a> {
    Null,
    Int4(i32),
    Int8(i64),
    Bool(bool),
    Text(&'a str),
}

impl Input<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Null => 0,
            Self::Int4(_) => 4,
            Self::Int8(_) => 8,
            Self::Bool(_) => 1,
            Self::Text(text) => text.len(),
        }
    }
}

fn input(value: &Value, column: Column) -> PgResult<Input<'_>> {
    match (value, column.oid) {
        (Value::Null, _) if column.nullable => Ok(Input::Null),
        (Value::Int(value), INT4) => i32::try_from(*value)
            .map(Input::Int4)
            .map_err(|_| SCHEMA_ERROR),
        (Value::Int(value), INT8) => Ok(Input::Int8(*value)),
        (Value::Bool(value), BOOL) => Ok(Input::Bool(*value)),
        (Value::String(value), TEXT | VARCHAR) => Ok(Input::Text(value)),
        _ => Err(SCHEMA_ERROR),
    }
}

enum Payload {
    Null,
    Int4([u8; 4]),
    Int8([u8; 8]),
    Bool([u8; 1]),
    Text(Vec<u8>),
}

pub(super) struct Param {
    pub(super) oid: u32,
    payload: Payload,
}

impl Param {
    pub(super) fn bytes(&self) -> Option<&[u8]> {
        Some(match &self.payload {
            Payload::Null => return None,
            Payload::Int4(bytes) => bytes,
            Payload::Int8(bytes) => bytes,
            Payload::Bool(bytes) => bytes,
            Payload::Text(bytes) => bytes,
        })
    }
}

fn read<T>(value: &Value, span: Span, f: impl FnOnce(&Value) -> PgResult<T>) -> PgResult<T> {
    value
        .with_read(span, |value| Ok(f(value)))
        .map_err(|_| SCHEMA_ERROR)?
}

pub(super) struct Arguments<'a> {
    pub(super) values: &'a [Value],
    pub(super) param_types: &'a [Value],
    pub(super) result_types: &'a [Value],
}

impl Arguments<'_> {
    pub(super) fn input_bytes(
        &self,
        sql_len: usize,
        span: Span,
        check: &impl Fn() -> PgResult<()>,
    ) -> PgResult<usize> {
        if self.values.len() > MAX_PARAM_COUNT
            || self.values.len() != self.param_types.len()
            || self.result_types.len() > MAX_COLUMNS
        {
            return Err(SCHEMA_ERROR);
        }
        for declared in self.result_types {
            check()?;
            Column::parse(declared, span)?;
        }
        let mut bytes = 0;
        for (value, declared) in self.values.iter().zip(self.param_types) {
            check()?;
            let column = Column::parse(declared, span)?;
            bytes += read(value, span, |value| {
                let value = input(value, column)?;
                let size = value.len();
                if size > MAX_PARAM_BYTES - bytes {
                    return Err(SCHEMA_ERROR);
                }
                if let Input::Text(text) = value {
                    check_for_nul(text, check, SCHEMA_ERROR)?;
                }
                Ok(size)
            })?;
        }
        // Charge the descriptor arena and libpq's parameter vectors before
        // they allocate. The binary scalar arrays are inline in each Param.
        let descriptor = size_of::<Param>() + size_of::<*const u8>() + 3 * size_of::<u32>();
        Ok(sql_len
            + 1
            + bytes
            + self.values.len() * descriptor
            + self.result_types.len() * size_of::<Column>())
    }

    pub(super) fn bind(
        &self,
        span: Span,
        check: &impl Fn() -> PgResult<()>,
    ) -> PgResult<(Vec<Param>, Vec<Column>)> {
        let mut columns = Vec::new();
        let mut params = Vec::new();
        check()?;
        columns
            .try_reserve_exact(self.result_types.len())
            .map_err(|_| OUT_OF_MEMORY)?;
        params
            .try_reserve_exact(self.values.len())
            .map_err(|_| OUT_OF_MEMORY)?;
        for declared in self.result_types {
            check()?;
            columns.push(Column::parse(declared, span)?);
        }
        for (value, declared) in self.values.iter().zip(self.param_types) {
            check()?;
            let column = Column::parse(declared, span)?;
            let payload = read(value, span, |value| {
                Ok(match input(value, column)? {
                    Input::Null => Payload::Null,
                    Input::Int4(value) => Payload::Int4(value.to_be_bytes()),
                    Input::Int8(value) => Payload::Int8(value.to_be_bytes()),
                    Input::Bool(value) => Payload::Bool([u8::from(value)]),
                    Input::Text(text) => {
                        let mut bytes = Vec::new();
                        bytes
                            .try_reserve_exact(text.len())
                            .map_err(|_| OUT_OF_MEMORY)?;
                        for chunk in text.as_bytes().chunks(INPUT_CHUNK) {
                            check()?;
                            bytes.extend_from_slice(chunk);
                        }
                        Payload::Text(bytes)
                    }
                })
            })?;
            params.push(Param {
                oid: column.oid,
                payload,
            });
        }
        Ok((params, columns))
    }
}

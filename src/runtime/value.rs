use std::{collections::HashMap, fmt};

use crate::ast::Stmt;
use crate::env::Env;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Array(Vec<Value>),
    Object(HashMap<String, Value>),
    Struct {
        name: String,
        fields: HashMap<String, Value>,
    },
    Enum {
        name: String,
        variant: String,
        fields: Vec<Value>,
    },
    Result {
        ok: bool,
        value: Box<Value>,
    },
    Function {
        params: Vec<String>,
        body: Vec<Stmt>,
        env: Env,
    },
    Null,
}

impl Value {
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(value) => *value,
            Value::Null => false,
            _ => true,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::String(_) => "str",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Struct { .. } => "struct",
            Value::Enum { .. } => "enum",
            Value::Result { .. } => "result",
            Value::Function { .. } => "function",
            Value::Null => "null",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(value) => write!(f, "{}", value),
            Value::Float(value) => write!(f, "{}", value),
            Value::Bool(value) => write!(f, "{}", value),
            Value::String(value) => write!(f, "{}", value),
            Value::Array(values) => {
                write!(f, "[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{value}")?;
                }
                write!(f, "]")
            }
            Value::Object(fields) => {
                write!(f, "{{ ")?;
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (field, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field}: {value}")?;
                }
                write!(f, " }}")
            }
            Value::Struct { name, fields } => {
                write!(f, "{name} {{ ")?;
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (field, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field}: {value}")?;
                }
                write!(f, " }}")
            }
            Value::Enum {
                name,
                variant,
                fields,
            } => {
                write!(f, "{name}.{variant}")?;
                if !fields.is_empty() {
                    write!(f, "(")?;
                    for (index, field) in fields.iter().enumerate() {
                        if index > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{field}")?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Value::Result { ok, value } => {
                if *ok {
                    write!(f, "Ok({value})")
                } else {
                    write!(f, "Err({value})")
                }
            }
            Value::Function { .. } => write!(f, "<function>"),
            Value::Null => write!(f, "null"),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(left), Value::Int(right)) => left == right,
            (Value::Float(left), Value::Float(right)) => left == right,
            (Value::Bool(left), Value::Bool(right)) => left == right,
            (Value::String(left), Value::String(right)) => left == right,
            (Value::Array(left), Value::Array(right)) => left == right,
            (Value::Object(left), Value::Object(right)) => left == right,
            (
                Value::Struct {
                    name: left_name,
                    fields: left_fields,
                },
                Value::Struct {
                    name: right_name,
                    fields: right_fields,
                },
            ) => left_name == right_name && left_fields == right_fields,
            (
                Value::Enum {
                    name: left_name,
                    variant: left_variant,
                    fields: left_fields,
                },
                Value::Enum {
                    name: right_name,
                    variant: right_variant,
                    fields: right_fields,
                },
            ) => {
                left_name == right_name
                    && left_variant == right_variant
                    && left_fields == right_fields
            }
            (
                Value::Result {
                    ok: left_ok,
                    value: left_value,
                },
                Value::Result {
                    ok: right_ok,
                    value: right_value,
                },
            ) => left_ok == right_ok && left_value == right_value,
            (Value::Function { .. }, Value::Function { .. }) => false,
            (Value::Null, Value::Null) => true,
            _ => false,
        }
    }
}

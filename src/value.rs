use std::{collections::HashMap, fmt};

use crate::ast::Stmt;

#[derive(Debug, Clone, PartialEq)]
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
    },
    Function {
        params: Vec<String>,
        body: Vec<Stmt>,
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
            Value::Enum { name, variant } => write!(f, "{name}.{variant}"),
            Value::Function { .. } => write!(f, "<function>"),
            Value::Null => write!(f, "null"),
        }
    }
}

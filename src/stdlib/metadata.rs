#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypePattern {
    Int,
    Bool,
    String,
    Unknown,
    Any,
    ArrayAny,
    ArrayOf(Box<TypePattern>),
    StringOrStringArray,
    ArrayElementOfArg(usize),
    ResultOf(Box<TypePattern>),
    SameAsArg(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArgRule {
    Is(TypePattern),
    MatchesArrayElement { array_arg: usize },
    MatchesArrayArg { array_arg: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signature {
    pub name: String,
    pub args: Vec<ArgRule>,
    pub returns: TypePattern,
    pub abi: CallAbi,
    pub failure: FailureMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallAbi {
    Builtin,
    DottedBuiltin { module: String, function: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailureMode {
    Never,
    ReturnsResult,
    MayPanic,
}

pub(crate) fn builtin_signature(name: &str) -> Option<Signature> {
    let args = match name {
        "len" | "str" | "ok" => vec![ArgRule::Is(TypePattern::Any)],
        "err" => vec![str_arg()],
        _ => return None,
    };
    let returns = match name {
        "len" => TypePattern::Int,
        "str" => TypePattern::String,
        "ok" => TypePattern::ResultOf(Box::new(TypePattern::SameAsArg(0))),
        "err" => TypePattern::ResultOf(Box::new(TypePattern::Unknown)),
        _ => return None,
    };
    Some(Signature {
        name: name.to_string(),
        args,
        returns,
        abi: CallAbi::Builtin,
        failure: match name {
            "ok" | "err" => FailureMode::ReturnsResult,
            _ => FailureMode::Never,
        },
    })
}

pub(crate) fn dotted_signature(module: &str, function: &str) -> Option<Signature> {
    let name = format!("{module}.{function}");
    let args = match (module, function) {
        ("fs", "read") => vec![str_arg()],
        ("fs", "try_read") => vec![str_arg()],
        ("lexer", "scan") => vec![str_arg()],
        ("parser", "parse") => vec![ArgRule::Is(TypePattern::StringOrStringArray)],
        ("string", "len" | "trim" | "lower" | "upper") => vec![str_arg()],
        ("string", "contains" | "starts_with" | "ends_with") => vec![str_arg(), str_arg()],
        ("string", "replace") => vec![str_arg(), str_arg(), str_arg()],
        ("string", "slice") => vec![str_arg(), int_arg(), int_arg()],
        ("array", "len" | "is_empty" | "first" | "last") => vec![array_arg()],
        ("array", "try_get") => vec![array_arg(), int_arg()],
        ("array", "push") => vec![array_arg(), ArgRule::MatchesArrayElement { array_arg: 0 }],
        ("array", "concat") => vec![array_arg(), ArgRule::MatchesArrayArg { array_arg: 0 }],
        ("json", "parse" | "try_parse") => vec![str_arg()],
        ("json", "stringify") => vec![ArgRule::Is(TypePattern::Any)],
        ("time", "now" | "unix" | "millis") => vec![],
        ("http", "try_get") => vec![str_arg()],
        _ => return None,
    };
    let returns = match (module, function) {
        ("fs", "read") => TypePattern::String,
        ("fs", "try_read") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("lexer", "scan") => TypePattern::ArrayOf(Box::new(TypePattern::String)),
        ("parser", "parse") => TypePattern::String,
        ("string", "len") => TypePattern::Int,
        ("string", "contains" | "starts_with" | "ends_with") => TypePattern::Bool,
        ("string", "slice") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("string", "trim" | "lower" | "upper" | "replace") => TypePattern::String,
        ("array", "len") => TypePattern::Int,
        ("array", "is_empty") => TypePattern::Bool,
        ("array", "first" | "last") => TypePattern::ArrayElementOfArg(0),
        ("array", "try_get") => TypePattern::ResultOf(Box::new(TypePattern::ArrayElementOfArg(0))),
        ("array", "push" | "concat") => TypePattern::SameAsArg(0),
        ("json", "parse") => TypePattern::Unknown,
        ("json", "try_parse") => TypePattern::ResultOf(Box::new(TypePattern::Unknown)),
        ("json", "stringify") => TypePattern::String,
        ("time", "now" | "unix" | "millis") => TypePattern::Int,
        ("http", "try_get") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        _ => return None,
    };
    Some(Signature {
        name,
        args,
        returns,
        abi: CallAbi::DottedBuiltin {
            module: module.to_string(),
            function: function.to_string(),
        },
        failure: dotted_failure_mode(module, function),
    })
}

fn dotted_failure_mode(module: &str, function: &str) -> FailureMode {
    match (module, function) {
        ("fs", "try_read")
        | ("string", "slice")
        | ("array", "try_get")
        | ("json", "try_parse")
        | ("http", "try_get") => FailureMode::ReturnsResult,
        ("fs", "read") | ("json", "parse") => FailureMode::MayPanic,
        _ => FailureMode::Never,
    }
}

pub(crate) fn module_requires_import(module: &str) -> bool {
    module == "http"
}

fn int_arg() -> ArgRule {
    ArgRule::Is(TypePattern::Int)
}

fn str_arg() -> ArgRule {
    ArgRule::Is(TypePattern::String)
}

fn array_arg() -> ArgRule {
    ArgRule::Is(TypePattern::ArrayAny)
}

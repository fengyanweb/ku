#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypePattern {
    Int,
    Bool,
    String,
    Null,
    Unknown,
    Any,
    ArrayAny,
    ObjectAny,
    ObjectFields(Vec<(String, TypePattern)>),
    ArrayOf(Box<TypePattern>),
    StringOrStringArray,
    ArrayElementOfArg(usize),
    ResultOf(Box<TypePattern>),
    SameAsArg(usize),
    /// A dynamic tagged value (e.g. `json.parse` result). Maps to `Type::KuValue`.
    KuValue,
    /// A backend-defined owned value. Most are move-only external handles (for
    /// example a database/net client); `__ku_bytes` is the deliberate cloneable
    /// exception. All remain opaque in Ku source and dispatch ownership through
    /// helpers selected by this synthetic type id.
    Native(&'static str),
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
        "len" | "str" | "ok" | "println" => vec![ArgRule::Is(TypePattern::Any)],
        "err" => vec![str_arg()],
        _ => return None,
    };
    let returns = match name {
        "len" => TypePattern::Int,
        "str" => TypePattern::String,
        "ok" => TypePattern::ResultOf(Box::new(TypePattern::SameAsArg(0))),
        "err" => TypePattern::ResultOf(Box::new(TypePattern::Unknown)),
        "println" => TypePattern::Null,
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
        ("fs", "write") => vec![str_arg(), str_arg()],
        ("fs", "try_write") => vec![str_arg(), str_arg()],
        ("fs", "exists" | "read_dir") => vec![str_arg()],
        ("lexer", "scan") => vec![str_arg()],
        ("parser", "parse") => vec![ArgRule::Is(TypePattern::StringOrStringArray)],
        ("string", "len" | "byte_len" | "chars" | "trim" | "lower" | "upper") => vec![str_arg()],
        ("string", "contains" | "starts_with" | "ends_with") => vec![str_arg(), str_arg()],
        ("string", "replace") => vec![str_arg(), str_arg(), str_arg()],
        ("string", "slice") => vec![str_arg(), int_arg(), int_arg()],
        ("array", "len" | "is_empty" | "first" | "last") => vec![array_arg()],
        ("array", "try_get") => vec![array_arg(), int_arg()],
        ("array", "push") => vec![array_arg(), ArgRule::MatchesArrayElement { array_arg: 0 }],
        ("array", "concat") => vec![array_arg(), ArgRule::MatchesArrayArg { array_arg: 0 }],
        ("object", "get_or") => vec![
            ArgRule::Is(TypePattern::ObjectAny),
            str_arg(),
            ArgRule::Is(TypePattern::Any),
        ],
        ("kuvalue", "as_int" | "as_str") => vec![ArgRule::Is(TypePattern::Any)],
        ("json", "parse" | "try_parse") => vec![str_arg()],
        ("json", "stringify") => vec![ArgRule::Is(TypePattern::Any)],
        ("config", "env") => vec![],
        ("config", "env_file") => vec![str_arg()],
        ("config", "yaml") => vec![str_arg()],
        ("time", "now" | "instant" | "unix" | "millis" | "steady_millis" | "date") => {
            vec![]
        }
        ("time", "elapsed") => vec![ArgRule::Is(TypePattern::ObjectAny)],
        ("time", "from_unix" | "from_millis" | "is_leap") => vec![int_arg()],
        ("time", "days_in_month") => vec![int_arg(), int_arg()],
        ("time", "sleep") => vec![ArgRule::Is(TypePattern::Any)],
        ("task", "stats") => vec![],
        ("task", "stress") => vec![int_arg(), int_arg(), int_arg()],
        ("http", "get") => vec![str_arg()],
        ("http", "post") => vec![str_arg(), str_arg()],
        ("http", "request") => vec![ArgRule::Is(TypePattern::ObjectAny)],
        ("http", "client" | "service" | "server") => vec![],
        ("http", "text") => vec![str_arg()],
        ("http", "html") => vec![str_arg()],
        ("http", "json") => vec![ArgRule::Is(TypePattern::Any)],
        ("http", "empty") => vec![],
        ("http", "redirect") => vec![str_arg()],
        ("http", "statusText") => vec![int_arg()],
        // std.pg exposes one ordinary business path: client(config), followed by
        // receiver methods. The client owns a bounded pool internally; raw
        // libpq connections are intentionally not part of the public API.
        ("pg", "client") => vec![ArgRule::Is(TypePattern::ObjectAny)],
        ("pg_client", "query") => vec![
            ArgRule::Is(TypePattern::Native(PG_CLIENT)),
            str_arg(),
            ArgRule::Is(TypePattern::ArrayOf(Box::new(TypePattern::String))),
        ],
        ("pg_result", "rows" | "cols") => vec![ArgRule::Is(TypePattern::Native(PG_RESULT))],
        ("pg_result", "value" | "is_null") => vec![
            ArgRule::Is(TypePattern::Native(PG_RESULT)),
            int_arg(),
            int_arg(),
        ],
        ("pg_client", "close") => vec![ArgRule::Is(TypePattern::Native(PG_CLIENT))],
        // std.mysql exposes one pooled client constructor. Queries are receiver
        // methods described under private synthetic modules so module-level
        // mysql.query/connect compatibility paths cannot be selected.
        ("mysql", "client") => vec![ArgRule::Is(TypePattern::ObjectAny)],
        // std.redis exposes one ordinary entry point. The returned client owns a
        // bounded, lazy connection pool; commands are receiver methods described
        // by `redis_client_method_signature`, not duplicate module functions.
        ("redis", "client") => vec![ArgRule::Is(TypePattern::ObjectAny)],
        // Binary data has one explicit construction boundary. `from_str` encodes
        // the string's UTF-8 bytes, while `from_array` validates every element as
        // one byte; receiver methods are intentionally not duplicated here.
        ("bytes", "from_str") => vec![str_arg()],
        ("bytes", "from_array") => vec![ArgRule::Is(TypePattern::ArrayOf(Box::new(
            TypePattern::Int,
        )))],
        // std.net exposes one move-only TCP client constructor. TLS will extend
        // this same config path rather than adding a parallel socket API.
        ("net", "client") => vec![ArgRule::Is(TypePattern::ObjectAny)],
        _ => return None,
    };
    let returns = match (module, function) {
        ("fs", "read") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("fs", "try_read") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("fs", "write") => TypePattern::ResultOf(Box::new(TypePattern::Null)),
        ("fs", "try_write") => TypePattern::ResultOf(Box::new(TypePattern::Null)),
        ("fs", "exists") => TypePattern::Bool,
        ("fs", "read_dir") => TypePattern::ResultOf(Box::new(TypePattern::ArrayOf(Box::new(
            TypePattern::String,
        )))),
        ("lexer", "scan") => TypePattern::ArrayOf(Box::new(TypePattern::String)),
        ("parser", "parse") => TypePattern::String,
        ("string", "len" | "byte_len") => TypePattern::Int,
        ("string", "chars") => TypePattern::ArrayOf(Box::new(TypePattern::String)),
        ("string", "contains" | "starts_with" | "ends_with") => TypePattern::Bool,
        ("string", "slice") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("string", "trim" | "lower" | "upper" | "replace") => TypePattern::String,
        ("array", "len") => TypePattern::Int,
        ("array", "is_empty") => TypePattern::Bool,
        ("array", "first" | "last") => TypePattern::ArrayElementOfArg(0),
        ("array", "try_get") => TypePattern::ResultOf(Box::new(TypePattern::ArrayElementOfArg(0))),
        ("array", "push" | "concat") => TypePattern::SameAsArg(0),
        ("object", "get_or") => TypePattern::Unknown,
        ("kuvalue", "as_int") => TypePattern::ResultOf(Box::new(TypePattern::Int)),
        ("kuvalue", "as_str") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("json", "parse") => TypePattern::ResultOf(Box::new(TypePattern::KuValue)),
        ("json", "try_parse") => TypePattern::ResultOf(Box::new(TypePattern::KuValue)),
        ("json", "stringify") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("config", "env" | "env_file") => TypePattern::ObjectAny,
        ("config", "yaml") => TypePattern::ResultOf(Box::new(TypePattern::ObjectAny)),
        ("time", "instant" | "date" | "from_unix" | "from_millis") => TypePattern::ObjectAny,
        ("time", "now" | "elapsed" | "unix" | "millis" | "steady_millis") => TypePattern::Int,
        ("time", "is_leap") => TypePattern::Bool,
        ("time", "days_in_month") => TypePattern::ResultOf(Box::new(TypePattern::Int)),
        ("time", "sleep") => TypePattern::ResultOf(Box::new(TypePattern::Null)),
        ("task", "stats") => task_stats_pattern(),
        ("task", "stress") => task_stress_pattern(),
        ("http", "get" | "post" | "request") => {
            TypePattern::ResultOf(Box::new(http_response_pattern()))
        }
        ("http", "client") => http_client_pattern(),
        ("http", "service" | "server") => http_service_pattern(),
        ("http", "text" | "html" | "json" | "empty" | "redirect") => http_response_pattern(),
        ("http", "statusText") => TypePattern::String,
        ("pg", "client") => TypePattern::ResultOf(Box::new(TypePattern::Native(PG_CLIENT))),
        ("pg_client", "query") => TypePattern::ResultOf(Box::new(TypePattern::Native(PG_RESULT))),
        ("pg_result", "rows" | "cols") => TypePattern::Int,
        ("pg_result", "value") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("pg_result", "is_null") => TypePattern::ResultOf(Box::new(TypePattern::Bool)),
        ("pg_client", "close") => TypePattern::Null,
        ("mysql", "client") => TypePattern::ResultOf(Box::new(TypePattern::Native(MYSQL_CLIENT))),
        ("redis", "client") => TypePattern::ResultOf(Box::new(TypePattern::Native(REDIS_CLIENT))),
        ("bytes", "from_str" | "from_array") => {
            TypePattern::ResultOf(Box::new(TypePattern::Native(BYTES)))
        }
        ("net", "client") => TypePattern::ResultOf(Box::new(TypePattern::Native(NET_CLIENT))),
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

/// Receiver-only API for the bounded MySQL client and detached result. Keeping
/// this out of `dotted_signature("mysql", ...)` makes the removed module-level
/// connect/query/query_params/close spellings hard errors.
pub(crate) fn mysql_method_signature(native: &str, function: &str) -> Option<Signature> {
    let (args, returns) = if native == MYSQL_CLIENT {
        match function {
            "query" => (
                vec![
                    ArgRule::Is(TypePattern::Native(MYSQL_CLIENT)),
                    str_arg(),
                    ArgRule::Is(TypePattern::ArrayOf(Box::new(TypePattern::String))),
                ],
                TypePattern::ResultOf(Box::new(TypePattern::Native(MYSQL_RESULT))),
            ),
            "execute" => (
                vec![
                    ArgRule::Is(TypePattern::Native(MYSQL_CLIENT)),
                    str_arg(),
                    ArgRule::Is(TypePattern::ArrayOf(Box::new(TypePattern::String))),
                ],
                TypePattern::ResultOf(Box::new(TypePattern::Int)),
            ),
            "close" => (
                vec![ArgRule::Is(TypePattern::Native(MYSQL_CLIENT))],
                TypePattern::Null,
            ),
            _ => return None,
        }
    } else if native == MYSQL_RESULT {
        match function {
            "rows" | "cols" => (
                vec![ArgRule::Is(TypePattern::Native(MYSQL_RESULT))],
                TypePattern::Int,
            ),
            "value" => (
                vec![
                    ArgRule::Is(TypePattern::Native(MYSQL_RESULT)),
                    int_arg(),
                    int_arg(),
                ],
                TypePattern::ResultOf(Box::new(TypePattern::String)),
            ),
            "is_null" => (
                vec![
                    ArgRule::Is(TypePattern::Native(MYSQL_RESULT)),
                    int_arg(),
                    int_arg(),
                ],
                TypePattern::ResultOf(Box::new(TypePattern::Bool)),
            ),
            _ => return None,
        }
    } else {
        return None;
    };
    Some(Signature {
        name: format!("mysql.{function}"),
        args,
        returns,
        abi: CallAbi::DottedBuiltin {
            module: "mysql".to_string(),
            function: function.to_string(),
        },
        failure: if matches!(function, "query" | "execute" | "value" | "is_null") {
            FailureMode::ReturnsResult
        } else {
            FailureMode::Never
        },
    })
}

/// Receiver-only API for the bounded Redis client. Keeping these signatures out
/// of `dotted_signature("redis", ...)` intentionally makes
/// `redis.get(client, key)` and the old raw-connection API unknown stdlib calls;
/// ordinary code has one path: `redis.client(config)?` followed by
/// `client.get(key)?` / `client.set(key, value)?`.
pub(crate) fn redis_client_method_signature(function: &str) -> Option<Signature> {
    let client = || ArgRule::Is(TypePattern::Native(REDIS_CLIENT));
    let args = match function {
        "ping" | "close" => vec![client()],
        "get" | "del" | "exists" => vec![client(), str_arg()],
        "set" => vec![client(), str_arg(), str_arg()],
        _ => return None,
    };
    let returns = match function {
        "ping" | "set" => TypePattern::ResultOf(Box::new(TypePattern::Null)),
        // GET is strict: RESP nil is redis/key_not_found. There is deliberately
        // no second `get_required` spelling and no nil-to-empty compatibility.
        "get" => TypePattern::ResultOf(Box::new(TypePattern::String)),
        "del" => TypePattern::ResultOf(Box::new(TypePattern::Int)),
        "exists" => TypePattern::ResultOf(Box::new(TypePattern::Bool)),
        "close" => TypePattern::Null,
        _ => return None,
    };
    Some(Signature {
        name: format!("redis.{function}"),
        args,
        returns,
        abi: CallAbi::DottedBuiltin {
            module: "redis".to_string(),
            function: function.to_string(),
        },
        failure: if function == "close" {
            FailureMode::Never
        } else {
            FailureMode::ReturnsResult
        },
    })
}

/// Receiver-only API for owned binary data. The value is cloneable and has no
/// external resource identity; `TypePattern::Native` is used only to keep the
/// ABI opaque to Ku source and avoid introducing a second public type spelling.
pub(crate) fn bytes_method_signature(function: &str) -> Option<Signature> {
    let bytes = || ArgRule::Is(TypePattern::Native(BYTES));
    let args = match function {
        "len" | "to_str" => vec![bytes()],
        "get" => vec![bytes(), int_arg()],
        _ => return None,
    };
    let returns = match function {
        "len" => TypePattern::Int,
        "get" => TypePattern::ResultOf(Box::new(TypePattern::Int)),
        "to_str" => TypePattern::ResultOf(Box::new(TypePattern::String)),
        _ => return None,
    };
    Some(Signature {
        name: format!("bytes.{function}"),
        args,
        returns,
        abi: CallAbi::DottedBuiltin {
            module: "bytes".to_string(),
            function: function.to_string(),
        },
        failure: if function == "len" {
            FailureMode::Never
        } else {
            FailureMode::ReturnsResult
        },
    })
}

/// Receiver-only API for one bounded plain-TCP transport. Reads and writes
/// borrow the move-only client; close consumes it. TLS will be selected through
/// `net.client(config)` once the native rustls runtime contract is fixed.
pub(crate) fn net_client_method_signature(function: &str) -> Option<Signature> {
    let client = || ArgRule::Is(TypePattern::Native(NET_CLIENT));
    let args = match function {
        "read" => vec![client(), int_arg()],
        "write" => vec![client(), ArgRule::Is(TypePattern::Native(BYTES))],
        "close" => vec![client()],
        _ => return None,
    };
    let returns = match function {
        "read" => TypePattern::ResultOf(Box::new(TypePattern::Native(BYTES))),
        "write" => TypePattern::ResultOf(Box::new(TypePattern::Null)),
        "close" => TypePattern::Null,
        _ => return None,
    };
    Some(Signature {
        name: format!("net.{function}"),
        args,
        returns,
        abi: CallAbi::DottedBuiltin {
            module: "net".to_string(),
            function: function.to_string(),
        },
        failure: if function == "close" {
            FailureMode::Never
        } else {
            FailureMode::ReturnsResult
        },
    })
}

fn dotted_failure_mode(module: &str, function: &str) -> FailureMode {
    match (module, function) {
        ("fs", "read" | "try_read" | "write" | "try_write" | "read_dir")
        | ("string", "slice")
        | ("array", "try_get")
        | ("kuvalue", "as_int" | "as_str")
        | ("json", "parse" | "try_parse" | "stringify")
        | ("config", "yaml")
        | ("time", "days_in_month" | "sleep")
        | ("http", "get" | "post" | "request")
        | ("pg", "client")
        | ("pg_client", "query")
        | ("pg_result", "value" | "is_null")
        | ("mysql", "client")
        | ("redis", "client")
        | ("bytes", "from_str" | "from_array")
        | ("net", "client") => FailureMode::ReturnsResult,
        ("config", "env_file") | ("task", "stats" | "stress") => FailureMode::MayPanic,
        _ => FailureMode::Never,
    }
}

/// Backend synthetic type ids for the opaque `pg` native handles.
pub(crate) const PG_RESULT: &str = "__ku_pg_result";
pub(crate) const PG_CLIENT: &str = "__ku_pg_client";
/// Backend synthetic type id for the opaque, bounded `redis` client/pool handle.
pub(crate) const REDIS_CLIENT: &str = "__ku_redis_client";
/// Backend synthetic type id for cloneable owned binary data.
pub(crate) const BYTES: &str = "__ku_bytes";
/// Backend synthetic type id for the move-only bounded TCP transport.
pub(crate) const NET_CLIENT: &str = "__ku_net_client";
/// Backend synthetic type ids for the pooled `mysql` client and detached result.
pub(crate) const MYSQL_CLIENT: &str = "__ku_mysql_client";
pub(crate) const MYSQL_RESULT: &str = "__ku_mysql_result";

pub(crate) fn module_requires_import(module: &str) -> bool {
    matches!(
        module,
        "fs" | "http" | "config" | "task" | "pg" | "redis" | "mysql" | "bytes" | "net"
    )
}

pub(crate) fn is_std_module(module: &str) -> bool {
    matches!(
        module,
        "fs" | "lexer"
            | "parser"
            | "string"
            | "array"
            | "object"
            | "json"
            | "config"
            | "time"
            | "task"
            | "http"
            | "pg"
            | "redis"
            | "mysql"
            | "bytes"
            | "net"
    )
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

fn http_response_pattern() -> TypePattern {
    TypePattern::ObjectFields(vec![
        ("status".to_string(), TypePattern::Int),
        ("headers".to_string(), TypePattern::ObjectAny),
        ("body".to_string(), TypePattern::String),
    ])
}

fn http_client_pattern() -> TypePattern {
    TypePattern::ObjectFields(vec![
        ("kind".to_string(), TypePattern::String),
        ("timeout_ms".to_string(), TypePattern::Int),
        ("max_body_bytes".to_string(), TypePattern::Int),
        ("max_idle_connections".to_string(), TypePattern::Int),
    ])
}

fn http_service_pattern() -> TypePattern {
    TypePattern::ObjectFields(vec![
        ("kind".to_string(), TypePattern::String),
        ("read_header_timeout_ms".to_string(), TypePattern::Int),
        ("read_body_timeout_ms".to_string(), TypePattern::Int),
        ("write_timeout_ms".to_string(), TypePattern::Int),
        ("idle_timeout_ms".to_string(), TypePattern::Int),
        ("handler_timeout_ms".to_string(), TypePattern::Int),
        ("max_body_bytes".to_string(), TypePattern::Int),
        ("max_header_bytes".to_string(), TypePattern::Int),
        ("max_connections".to_string(), TypePattern::Int),
        ("max_active_requests".to_string(), TypePattern::Int),
        ("max_pending_requests".to_string(), TypePattern::Int),
        (
            "routes".to_string(),
            TypePattern::ArrayOf(Box::new(http_route_pattern())),
        ),
    ])
}

fn http_route_pattern() -> TypePattern {
    TypePattern::ObjectFields(vec![
        ("method".to_string(), TypePattern::String),
        ("path".to_string(), TypePattern::String),
        (
            "param_names".to_string(),
            TypePattern::ArrayOf(Box::new(TypePattern::String)),
        ),
        ("handler".to_string(), TypePattern::Any),
    ])
}

fn task_stats_pattern() -> TypePattern {
    TypePattern::ObjectFields(vec![
        ("active_tasks".to_string(), TypePattern::Int),
        ("registered_tasks".to_string(), TypePattern::Int),
        ("queued_tasks".to_string(), TypePattern::Int),
        ("wait_edges".to_string(), TypePattern::Int),
        ("queued_blocking_jobs".to_string(), TypePattern::Int),
        ("running_blocking_jobs".to_string(), TypePattern::Int),
        ("task_workers".to_string(), TypePattern::Int),
        ("blocking_workers".to_string(), TypePattern::Int),
        ("total_submissions".to_string(), TypePattern::Int),
        ("accepted_submissions".to_string(), TypePattern::Int),
        ("rejected_task_limit".to_string(), TypePattern::Int),
        ("rejected_task_queue".to_string(), TypePattern::Int),
        ("rejected_task_internal".to_string(), TypePattern::Int),
        ("finished_tasks".to_string(), TypePattern::Int),
    ])
}

fn task_stress_pattern() -> TypePattern {
    TypePattern::ObjectFields(vec![
        ("demand".to_string(), TypePattern::Int),
        ("producers".to_string(), TypePattern::Int),
        ("hold_ms".to_string(), TypePattern::Int),
        ("peak_active".to_string(), TypePattern::Int),
        ("accepted".to_string(), TypePattern::Int),
        ("rejected_limit".to_string(), TypePattern::Int),
        ("rejected_queue".to_string(), TypePattern::Int),
        ("rejected_internal".to_string(), TypePattern::Int),
        ("finished".to_string(), TypePattern::Int),
        ("submit_ms".to_string(), TypePattern::Int),
        ("total_ms".to_string(), TypePattern::Int),
        ("task_workers".to_string(), TypePattern::Int),
        ("blocking_workers".to_string(), TypePattern::Int),
    ])
}

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
    /// An opaque native handle owned by a C-library binding (e.g. a `pg` connection
    /// or result). Maps to `Type::Native(name)` — owned, move-tracked, dropped by the
    /// backend (which closes/frees the underlying C resource). The name carries the
    /// backend's synthetic type id (e.g. `__ku_pg_conn`).
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
        ("time", "now" | "unix" | "millis" | "date") => vec![],
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
        // std.pg — thin libpq binding. Conn/result are opaque owned native handles.
        ("pg", "connect") => vec![str_arg()],
        ("pg", "query") => vec![ArgRule::Is(TypePattern::Native(PG_CONN)), str_arg()],
        ("pg", "query_params") => vec![
            ArgRule::Is(TypePattern::Native(PG_CONN)),
            str_arg(),
            ArgRule::Is(TypePattern::ArrayOf(Box::new(TypePattern::String))),
        ],
        ("pg", "rows" | "cols") => vec![ArgRule::Is(TypePattern::Native(PG_RESULT))],
        ("pg", "value") => vec![
            ArgRule::Is(TypePattern::Native(PG_RESULT)),
            int_arg(),
            int_arg(),
        ],
        ("pg", "close") => vec![ArgRule::Is(TypePattern::Native(PG_CONN))],
        // Connection pool: the pool owns the connections; queries borrow one
        // internally and return it, so a caller can never leak a connection.
        ("pg", "pool") => vec![str_arg(), int_arg()],
        ("pg", "pool_query") => vec![ArgRule::Is(TypePattern::Native(PG_POOL)), str_arg()],
        ("pg", "pool_query_params") => vec![
            ArgRule::Is(TypePattern::Native(PG_POOL)),
            str_arg(),
            ArgRule::Is(TypePattern::ArrayOf(Box::new(TypePattern::String))),
        ],
        ("pg", "pool_close") => vec![ArgRule::Is(TypePattern::Native(PG_POOL))],
        // std.mysql — thin libmysqlclient binding. Conn/result are opaque owned handles.
        ("mysql", "connect") => vec![str_arg(), int_arg(), str_arg(), str_arg(), str_arg()],
        ("mysql", "query") => vec![ArgRule::Is(TypePattern::Native(MYSQL_CONN)), str_arg()],
        ("mysql", "query_params") => vec![
            ArgRule::Is(TypePattern::Native(MYSQL_CONN)),
            str_arg(),
            ArgRule::Is(TypePattern::ArrayOf(Box::new(TypePattern::String))),
        ],
        ("mysql", "rows" | "cols") => vec![ArgRule::Is(TypePattern::Native(MYSQL_RESULT))],
        ("mysql", "value") => vec![
            ArgRule::Is(TypePattern::Native(MYSQL_RESULT)),
            int_arg(),
            int_arg(),
        ],
        ("mysql", "close") => vec![ArgRule::Is(TypePattern::Native(MYSQL_CONN))],
        // std.redis — thin RESP-over-socket binding. Conn is an opaque owned handle.
        ("redis", "connect") => vec![str_arg(), int_arg()],
        ("redis", "auth") => vec![ArgRule::Is(TypePattern::Native(REDIS_CONN)), str_arg()],
        ("redis", "get") => vec![ArgRule::Is(TypePattern::Native(REDIS_CONN)), str_arg()],
        ("redis", "set") => vec![
            ArgRule::Is(TypePattern::Native(REDIS_CONN)),
            str_arg(),
            str_arg(),
        ],
        ("redis", "del") => vec![ArgRule::Is(TypePattern::Native(REDIS_CONN)), str_arg()],
        ("redis", "close") => vec![ArgRule::Is(TypePattern::Native(REDIS_CONN))],
        _ => return None,
    };
    let returns = match (module, function) {
        ("fs", "read") => TypePattern::String,
        ("fs", "try_read") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("fs", "write") => TypePattern::Null,
        ("fs", "try_write") => TypePattern::ResultOf(Box::new(TypePattern::Null)),
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
        ("object", "get_or") => TypePattern::Unknown,
        ("kuvalue", "as_int") => TypePattern::ResultOf(Box::new(TypePattern::Int)),
        ("kuvalue", "as_str") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("json", "parse") => TypePattern::KuValue,
        ("json", "try_parse") => TypePattern::ResultOf(Box::new(TypePattern::KuValue)),
        ("json", "stringify") => TypePattern::String,
        ("config", "env" | "env_file") => TypePattern::ObjectAny,
        ("config", "yaml") => TypePattern::ResultOf(Box::new(TypePattern::ObjectAny)),
        ("time", "now" | "date" | "from_unix" | "from_millis") => TypePattern::ObjectAny,
        ("time", "unix" | "millis") => TypePattern::Int,
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
        ("pg", "connect") => TypePattern::ResultOf(Box::new(TypePattern::Native(PG_CONN))),
        ("pg", "query" | "query_params") => {
            TypePattern::ResultOf(Box::new(TypePattern::Native(PG_RESULT)))
        }
        ("pg", "rows" | "cols") => TypePattern::Int,
        ("pg", "value") => TypePattern::String,
        ("pg", "close") => TypePattern::Null,
        ("pg", "pool") => TypePattern::ResultOf(Box::new(TypePattern::Native(PG_POOL))),
        ("pg", "pool_query" | "pool_query_params") => {
            TypePattern::ResultOf(Box::new(TypePattern::Native(PG_RESULT)))
        }
        ("pg", "pool_close") => TypePattern::Null,
        ("mysql", "connect") => TypePattern::ResultOf(Box::new(TypePattern::Native(MYSQL_CONN))),
        ("mysql", "query" | "query_params") => {
            TypePattern::ResultOf(Box::new(TypePattern::Native(MYSQL_RESULT)))
        }
        ("mysql", "rows" | "cols") => TypePattern::Int,
        ("mysql", "value") => TypePattern::String,
        ("mysql", "close") => TypePattern::Null,
        ("redis", "connect") => TypePattern::ResultOf(Box::new(TypePattern::Native(REDIS_CONN))),
        ("redis", "auth" | "set") => TypePattern::ResultOf(Box::new(TypePattern::Null)),
        ("redis", "get") => TypePattern::ResultOf(Box::new(TypePattern::String)),
        ("redis", "del") => TypePattern::ResultOf(Box::new(TypePattern::Int)),
        ("redis", "close") => TypePattern::Null,
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
        ("fs", "try_read" | "try_write")
        | ("string", "slice")
        | ("array", "try_get")
        | ("kuvalue", "as_int" | "as_str")
        | ("json", "try_parse")
        | ("config", "yaml")
        | ("time", "days_in_month" | "sleep")
        | ("http", "get" | "post" | "request")
        | ("pg", "connect" | "query" | "query_params" | "pool" | "pool_query" | "pool_query_params")
        | ("mysql", "connect" | "query" | "query_params")
        | ("redis", "connect" | "auth" | "get" | "set" | "del") => FailureMode::ReturnsResult,
        ("fs", "read" | "write")
        | ("json", "parse")
        | ("config", "env_file")
        | ("task", "stats" | "stress") => FailureMode::MayPanic,
        _ => FailureMode::Never,
    }
}

/// Backend synthetic type ids for the opaque `pg` native handles.
pub(crate) const PG_CONN: &str = "__ku_pg_conn";
pub(crate) const PG_RESULT: &str = "__ku_pg_result";
pub(crate) const PG_POOL: &str = "__ku_pg_pool";
/// Backend synthetic type id for the opaque `redis` connection handle.
pub(crate) const REDIS_CONN: &str = "__ku_redis_conn";
/// Backend synthetic type ids for the opaque `mysql` handles.
pub(crate) const MYSQL_CONN: &str = "__ku_mysql_conn";
pub(crate) const MYSQL_RESULT: &str = "__ku_mysql_result";

pub(crate) fn module_requires_import(module: &str) -> bool {
    matches!(module, "fs" | "http" | "config" | "task" | "pg" | "redis" | "mysql")
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

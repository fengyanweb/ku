/**
 * Compact editor metadata audited against the 2026-11-22 Ku 0.0.20 SDK
 * source-264e53d44f21. Sources are repository-relative, not files read at runtime.
 * metadata.rs supplies ordinary signatures; checker/mod.rs and the implementations
 * below refine overloaded time calls, HTTP control methods and native PG handles.
 * This is editor assistance, not a second compiler or an SDK-directory scanner.
 */
export interface ApiParameter { name: string; type: string; optional?: boolean }
export interface ApiSignature {
  label: string;
  parameters: readonly ApiParameter[];
  returns: string;
  documentation: string;
  source: string;
}
export interface ApiMember {
  name: string;
  type: string;
  kind: "field" | "method";
  signature?: ApiSignature;
}

type ParameterSpec = readonly [name: string, type: string, optional?: boolean];
const signatures = new Map<string, ApiSignature[]>();
const shapes = new Map<string, ApiMember[]>();
function define(name: string, parameters: readonly ParameterSpec[], returns: string, documentation: string, source: string) {
  const params = parameters.map(([name, type, optional]) => ({ name, type, ...(optional ? { optional } : {}) }));
  const signature: ApiSignature = { label: `${name}(${params.map((p) => `${p.name}${p.optional ? "?" : ""}: ${p.type}`).join(", ")}): ${returns}`,
    parameters: params, returns, documentation, source };
  signatures.set(name, [...(signatures.get(name) ?? []), signature]);
  return signature;
}
function fields(type: string, entries: readonly ParameterSpec[]) {
  shapes.set(type, entries.map(([name, type]) => ({ name, type, kind: "field" })));
}
function method(type: string, name: string, params: readonly ParameterSpec[], returns: string, documentation: string, source: string) {
  const signature = define(`${type}.${name}`, params, returns, documentation, source);
  const members = shapes.get(type) ?? [];
  members.push({ name, type: `fn(${signature.parameters.map((p) => p.type).join(", ")}): ${returns}`, kind: "method", signature });
  shapes.set(type, members);
}
const TIME = "src/stdlib/time.rs; src/checker/mod.rs";
const ARRAY = "src/stdlib/array.rs; src/stdlib/metadata.rs";
const HTTP = "src/stdlib/http.rs; src/checker/mod.rs";
const PG = "src/stdlib/pg/mod.rs; src/stdlib/pg/typed.rs; src/stdlib/metadata.rs";
const META = "src/stdlib/metadata.rs";

fields("Time", [["kind", "str"], ["millis", "int"]]);
fields("Duration", [["kind", "str"], ["millis", "int"]]);
fields("Date", [["kind", "str"], ["year", "int"], ["month", "int"], ["day", "int"]]);
fields("TimeParts", [["kind", "str"], ["year", "int"], ["month", "int"], ["day", "int"], ["hour", "int"], ["minute", "int"], ["second", "int"], ["millis", "int"], ["weekday", "int"], ["zone", "str"]]);
define("time.now", [], "int", "Current Unix milliseconds, not a Time object.", TIME);
define("time.instant", [], "Time", "Time object tagged time.time with Unix millis.", TIME);
define("time.steady_millis", [], "int", "Process-local monotonic milliseconds for elapsed-time measurement.", TIME);
define("time.elapsed", [["previous", "Time"]], "int", "Wall-clock milliseconds since previous Time; use steady_millis for monotonic timing.", TIME);
define("time.unix", [["value", "Time", true]], "int", "Unix seconds; omitting value reads the current clock.", TIME);
define("time.millis", [["value", "Time|Duration", true]], "int", "Read milliseconds; no argument reads the current clock.", TIME);
define("time.from_unix", [["seconds", "int"]], "Time", "Construct Time from Unix seconds.", TIME);
define("time.from_millis", [["millis", "int"]], "Time", "Construct Time from Unix milliseconds.", TIME);
define("time.date", [], "Date", "Today's local calendar Date. This zero-argument overload is not Result.", TIME);
define("time.date", [["value", "Time"], ["zone", "str", true]], "Date!", "Extract a calendar Date; propagate or catch conversion/zone errors.", TIME);
define("time.date", [["year", "int"], ["month", "int"], ["day", "int"]], "Date!", "Validate and construct a calendar Date.", TIME);
define("time.datetime", [["year", "int"], ["month", "int"], ["day", "int"], ["hour", "int"], ["minute", "int"], ["second", "int"], ["zone", "str", true]], "Time!", "Construct Time from calendar fields, optionally in an IANA time zone.", TIME);
define("time.duration", [["value", "int"], ["unit", "str", true]], "Duration!", "Nonnegative duration. Units: ms (default), s, m, h, d.", TIME);
define("time.format", [["value", "Time"]], "str", "Format with the default layout. This overload is not Result.", TIME);
define("time.format", [["value", "Time"], ["layout", "str"], ["zone", "str", true]], "str!", "Format with layout tokens yyyy/MM/dd/HH/mm/ss/SSS; explicit layout/zone may fail.", TIME);
define("time.parse", [["text", "str"], ["layout", "str", true], ["zone", "str", true]], "Time!", "Parse time using the default or supplied layout and optional time zone.", TIME);
for (const name of ["add", "sub"]) define(`time.${name}`, [["value", "Time"], ["duration", "Duration"]], "Time", "Return a new Time; inputs remain readable.", TIME);
define("time.diff", [["later", "Time"], ["earlier", "Time"]], "Duration", "Signed duration between two Time values.", TIME);
define("time.compare", [["left", "Time"], ["right", "Time"]], "int", "Compare Time values: -1, 0 or 1.", TIME);
define("time.parts", [["value", "Time"]], "TimeParts", "Local calendar fields. This overload is not Result.", TIME);
define("time.parts", [["value", "Time"], ["zone", "str"]], "TimeParts!", "Calendar fields in an explicit zone; conversion may fail.", TIME);
define("time.weekday", [["value", "Date|Time"]], "int", "Weekday, Monday=1 through Sunday=7.", TIME);
define("time.weekday", [["value", "Date|Time"], ["zone", "str"]], "int!", "Weekday in an explicit time zone; conversion may fail.", TIME);
define("time.is_leap", [["year", "int"]], "bool", "Whether year is a leap year.", TIME);
define("time.days_in_month", [["year", "int"], ["month", "int"]], "int!", "Validate year/month and return the number of days.", TIME);
define("time.sleep", [["duration", "int|Duration"]], "null!", "Pause for milliseconds or Duration; supports use inside async fn.", TIME);

const arrayMethods: Array<[string, ParameterSpec[], string, string]> = [
  ["len", [], "int", "Number of elements."], ["is_empty", [], "bool", "Whether the array is empty."],
  ["first", [], "T|null", "First element, or null for an empty array; not Result and does not consume the array."],
  ["last", [], "T|null", "Last element, or null for an empty array; not Result and does not consume the array."],
  ["try_get", [["index", "int"]], "T!", "Element at index, or a recoverable bounds error."],
  ["push", [["value", "T"]], "[T]", "Return a new array with one appended value; the original array remains readable."],
  ["concat", [["other", "[T]"]], "[T]", "Return both arrays concatenated without consuming either input."],
];
for (const [name, params, returns, docs] of arrayMethods) {
  define(`array.${name}`, [["values", "[T]"], ...params], returns, docs, ARRAY);
  method("Array", name, params, returns, docs, ARRAY);
}
method("Array", "map", [["mapper", "fn(T): U"]], "[U]", "Map each element. Receiver method only; array.map is not a module function.", "src/checker/mod.rs; src/runtime/interpreter.rs");
for (const name of ["len", "byte_len", "chars", "trim", "lower", "upper", "contains", "starts_with", "ends_with", "replace", "slice"]) {
  const params: ParameterSpec[] = name === "replace" ? [["from", "str"], ["to", "str"]]
    : name === "slice" ? [["start", "int"], ["end", "int"]]
      : ["contains", "starts_with", "ends_with"].includes(name) ? [["text", "str"]] : [];
  const returns = ["len", "byte_len"].includes(name) ? "int" : name === "chars" ? "[str]" : ["contains", "starts_with", "ends_with"].includes(name) ? "bool" : name === "slice" ? "str!" : "str";
  define(`string.${name}`, [["value", "str"], ...params], returns, "String helper. len counts Unicode scalars; byte_len counts UTF-8 bytes.", META);
  method("str", name, params, returns, "String receiver method.", META);
}
method("str", "clone", [], "str", "Explicit owned copy of a string.", "src/checker/mod.rs");
method("Array", "clone", [], "[T]", "Explicit owned copy of an array without Task values.", "src/checker/mod.rs");

fields("Error", [["domain", "str"], ["code", "str"], ["message", "str"]]);
fields("HttpRequest", [["method", "str"], ["path", "str"], ["params", "StringMap"], ["query", "StringMap"], ["headers", "StringMap"], ["body", "str"]]);
fields("HttpResponse", [["status", "int"], ["headers", "StringMap"], ["body", "str"]]);
fields("HttpRequestConfig", [["url", "str"], ["method", "str"], ["headers", "StringMap"], ["body", "str"], ["timeout_ms", "int"], ["max_body_bytes", "int"]]);
fields("HttpClientConfig", [["timeout_ms", "int"], ["max_body_bytes", "int"], ["max_idle_connections", "int"]]);
// The runtime stores max_idle_connections, but this SDK's checker exposes it
// only as constructor input. Do not suggest a rejected receiver projection.
fields("HttpClient", [["kind", "str"], ["timeout_ms", "int"], ["max_body_bytes", "int"]]);
fields("HttpServiceConfig", ["read_header_timeout_ms", "read_body_timeout_ms", "write_timeout_ms", "idle_timeout_ms", "handler_timeout_ms", "max_body_bytes", "max_header_bytes", "max_connections", "max_active_requests", "max_pending_requests", "max_buffer_bytes"].map((name): ParameterSpec => [name, "int"]));
fields("HttpService", [["kind", "str"], ...(shapes.get("HttpServiceConfig") ?? []).map((m): ParameterSpec => [m.name, m.type]), ["routes", "[HttpRoute]"]]);
fields("HttpRoute", [["method", "str"], ["path", "str"], ["param_names", "[str]"], ["handler", "unknown"]]);
fields("HttpListener", [["kind", "str"], ["address", "str"]]);
define("http.get", [["url", "str"]], "HttpResponse!", "HTTP GET; propagate or catch transport errors. Does not return a Task by itself.", HTTP);
define("http.post", [["url", "str"], ["body", "str"]], "HttpResponse!", "HTTP POST with a string body.", HTTP);
define("http.request", [["config", "HttpRequestConfig"]], "HttpResponse!", "HTTP request using method/url/headers/body and bounded time/body options.", HTTP);
define("http.client", [["config", "HttpClientConfig", true]], "HttpClient", "Client configuration value; not a client with get/post methods.", HTTP);
for (const name of ["service", "server"]) define(`http.${name}`, [["config", "HttpServiceConfig", true]], "HttpService", "HTTP service with bounded defaults. Register handlers, then bind or listen.", HTTP);
for (const name of ["text", "html", "json"]) {
  const body = name === "json" ? "unknown" : "str";
  define(`http.${name}`, [["body", body]], "HttpResponse", "Construct a response with status 200.", HTTP);
  define(`http.${name}`, [["status", "int"], ["body", body]], "HttpResponse", "Construct an ordinary response; handlers return it rather than writing through a response parameter.", HTTP);
}
define("http.empty", [["status", "int", true]], "HttpResponse", "Construct an empty response (default status 204).", HTTP);
define("http.redirect", [["location", "str"]], "HttpResponse", "Construct a redirect response (default status 302).", HTTP);
define("http.redirect", [["status", "int"], ["location", "str"]], "HttpResponse", "Construct a redirect response with an explicit 3xx status.", HTTP);
define("http.statusText", [["status", "int"]], "str", "HTTP reason phrase for a status code.", HTTP);
// Keep this public constant set aligned with HTTP_STATUS_CODES/HTTP_CODE_ALIASES.
fields("HttpStatus", ["ok", "created", "accepted", "noContent", "movedPermanently", "found", "seeOther", "notModified", "temporaryRedirect", "permanentRedirect", "badRequest", "unauthorized", "forbidden", "notFound", "methodNotAllowed", "notAcceptable", "requestTimeout", "conflict", "gone", "contentTooLarge", "uriTooLong", "unsupportedMedia", "rangeNotSatisfiable", "expectationFailed", "unprocessable", "tooManyRequests", "headerTooLarge", "internalError", "notImplemented", "badGateway", "serviceUnavailable", "gatewayTimeout"].map((name): ParameterSpec => [name, "int"]));
fields("HttpCode", ["SUCCESS", "CREATED", "ACCEPTED", "NO_CONTENT", "BAD_REQUEST", "UNAUTHORIZED", "FORBIDDEN", "NOT_FOUND", "VALIDATION_FAILED", "INTERNAL_ERROR"].map((name): ParameterSpec => [name, "int"]));
for (const name of ["get", "post", "put", "del"]) method("HttpService", name, [["path", "str"], ["handler", "HttpHandler"]], "null", "Register fn() or fn(req: HttpRequest), returning HttpResponse or HttpResponse!. async fn handlers return a one-use Task<HttpResponse!> accepted by registration.", HTTP);
method("HttpService", "bind", [["address", "str"]], "HttpListener!", "Bind and return a listener without running its accept loop.", HTTP);
method("HttpService", "listen", [["address", "str"]], "null!", "Run the service until completion; consumes the service on the native path.", HTTP);
method("HttpListener", "run", [], "null!", "Run the bound listener.", HTTP);
method("HttpListener", "close", [], "null!", "Close the bound listener and release its resources.", HTTP);

fields("PgClientConfig", [["conninfo", "str"], ["max_connections", "int"], ["max_waiters", "int"], ["connect_timeout_ms", "int"], ["acquire_timeout_ms", "int"], ["query_timeout_ms", "int"]]);
define("pg.client", [["config", "PgClientConfig"]], "PgClient!", "Create a bounded pooled PostgreSQL client. Keep credentials outside source files.", PG);
method("PgClient", "query", [["sql", "str"], ["params", "[str]"]], "PgResult!", "Ordinary parameterized PG query. Use $1 placeholders and string parameters; do not concatenate untrusted SQL. In async fn the same call uses the runtime async path.", PG);
method("PgClient", "query_one_typed", [["sql", "str"], ["params", "[KuValue]"], ["param_types", "[str]"], ["result_types", "[str]"]], "PgResult!", "Exactly one binary row; schemas accept int4/int8/bool/text/varchar with optional ? suffix. No implicit retry and no generic Row type argument.", PG);
method("PgClient", "close", [], "null", "Close the pooled client. A detached PgResult remains readable; close is not Result.", PG);
for (const name of ["rows", "cols"]) method("PgResult", name, [], "int", "Number of detached result rows/columns.", PG);
for (const [name, returns] of [["value", "str!"], ["value_int", "int!"], ["value_bool", "bool!"], ["is_null", "bool!"]]) method("PgResult", name, [["row", "int"], ["column", "int"]], returns, "Read a result cell by zero-based row and column. Propagate or catch range/type/NULL errors.", PG);
for (const [name, returns] of [["as_int", "int!"], ["as_str", "str!"]]) method("KuValue", name, [], returns, "Checked conversion from a dynamic JSON value.", META);
method("KuValue", "take", [["key", "str"]], "KuValue!", "Take an owned field from a dynamic JSON object.", META);
for (const name of ["parse", "decode", "try_parse"]) define(`json.${name}`, [["text", "str"]], "KuValue!", "Parse JSON into an owned dynamic value. Use checked conversions such as as_str/as_int.", META);
for (const name of ["stringify", "encode"]) define(`json.${name}`, [["value", "unknown"]], "str!", "Encode a value to a JSON string.", META);
for (const [name, params, returns] of [
  ["len", [["value", "str|[T]"]], "int"], ["str", [["value", "unknown"]], "str"],
  ["ok", [["value", "T"]], "T!"], ["err", [["message", "str"]], "unknown!"],
  ["println", [["value", "unknown"]], "null"],
] as Array<[string, ParameterSpec[], string]>) define(name, params, returns, "Ku built-in function.", META);
define("object.get_or", [["object", "object"], ["key", "str"], ["fallback", "unknown"]], "unknown", "Explicit permissive field lookup; fallback is evaluated eagerly.", META);
for (const [name, params, returns] of [
  ["read", [["path", "str"]], "str!"], ["try_read", [["path", "str"]], "str!"],
  ["write", [["path", "str"], ["text", "str"]], "null!"], ["try_write", [["path", "str"], ["text", "str"]], "null!"],
  ["exists", [["path", "str"]], "bool"], ["read_dir", [["path", "str"]], "[str]!"],
] as Array<[string, ParameterSpec[], string]>) define(`fs.${name}`, params, returns, "File-system helper.", META);
define("bytes.from_str", [["text", "str"]], "Bytes!", "Encode a string as owned UTF-8 bytes.", META);
define("bytes.from_array", [["values", "[int]"]], "Bytes!", "Construct bytes from integers in the 0..255 range.", META);
method("Bytes", "len", [], "int", "Number of bytes.", META);
method("Bytes", "to_str", [], "str!", "Decode bytes as UTF-8.", META);
method("Bytes", "get", [["index", "int"]], "int!", "Read a byte by zero-based index.", META);
method("Bytes", "clone", [], "Bytes", "Owned copy of the binary data.", META);
for (const module of ["zstd", "gzip", "deflate"]) {
  for (const name of ["compress", "decompress"]) define(`${module}.${name}`, [["data", "Bytes"], ["max_output_bytes", "int"]], "Bytes!", "Native codec operation with an explicit output byte limit; the second argument is a size bound, not a compression level.", "src/stdlib/metadata.rs; src/backend/c_compression.rs");
}
define("lexer.scan", [["source", "str"]], "[str]", "Scan source into token strings.", META);
define("parser.parse", [["source", "str|[str]"]], "str", "Parse source as debug text; a token-string array produces a count summary.", META);
define("config.env", [], "object", "Read environment variables.", META);
define("config.env_file", [["path", "str"]], "object", "Read an environment file.", META);
define("config.yaml", [["path", "str"]], "object!", "Read YAML configuration.", META);
define("mysql.client", [["config", "object"]], "MysqlClient!", "Create the pooled MySQL client; raw connect/query module APIs are not public.", META);
define("redis.client", [["config", "object"]], "RedisClient!", "Create the pooled Redis client; commands are receiver methods.", META);
define("net.client", [["config", "object"]], "NetClient!", "Create a bounded native TCP/TLS client.", META);
for (const [type, name, params, returns] of [
  ["MysqlClient", "query", [["sql", "str"], ["params", "[str]"]], "MysqlResult!"],
  ["MysqlClient", "execute", [["sql", "str"], ["params", "[str]"]], "int!"],
  ["MysqlClient", "close", [], "null"],
  ["MysqlResult", "rows", [], "int"], ["MysqlResult", "cols", [], "int"],
  ["MysqlResult", "value", [["row", "int"], ["column", "int"]], "str!"],
  ["MysqlResult", "is_null", [["row", "int"], ["column", "int"]], "bool!"],
  ["RedisClient", "ping", [], "null!"], ["RedisClient", "get", [["key", "str"]], "str!"],
  ["RedisClient", "set", [["key", "str"], ["value", "str"]], "null!"],
  ["RedisClient", "del", [["key", "str"]], "int!"], ["RedisClient", "exists", [["key", "str"]], "bool!"],
  ["RedisClient", "close", [], "null"], ["NetClient", "read", [["maximum_bytes", "int"]], "Bytes!"],
  ["NetClient", "write", [["data", "Bytes"]], "null!"], ["NetClient", "close", [], "null"],
] as Array<[string, string, ParameterSpec[], string]>) method(type, name, params, returns, "Native client/result receiver method; errors use Ku Result.", META);
fields("TaskStats", ["active_tasks", "registered_tasks", "queued_tasks", "wait_edges", "queued_blocking_jobs", "running_blocking_jobs", "resident_blocking_jobs", "peak_resident_blocking_jobs", "peak_queued_blocking_jobs", "peak_running_blocking_jobs", "accepted_blocking_jobs", "rejected_blocking_jobs", "rejected_blocking_queue", "finished_blocking_jobs", "task_workers", "blocking_workers", "total_submissions", "accepted_submissions", "rejected_task_limit", "rejected_task_queue", "rejected_task_internal", "finished_tasks", "finished_cancelled_tasks", "finished_timed_out_tasks", "suppressed_cleanup_outcomes", "cleanup_timeouts", "cleanup_unfinished_tasks"].map((name): ParameterSpec => [name, "int"]));
fields("TaskStress", ["demand", "producers", "hold_ms", "peak_active", "accepted", "rejected_limit", "rejected_queue", "rejected_internal", "finished", "submit_ms", "total_ms", "task_workers", "blocking_workers"].map((name): ParameterSpec => [name, "int"]));
define("task.stats", [], "TaskStats", "Runtime diagnostic counters; does not inspect an ordinary async Task handle.", META);
define("task.stress", [["demand", "int"], ["producers", "int"], ["hold_ms", "int"]], "TaskStress", "Explicit bounded diagnostic workload; never run automatically by the editor.", META);

const moduleShapes = new Map<string, ApiMember[]>();
for (const [path, overloads] of signatures) {
  const parts = /^([a-z][a-z0-9_]*)\.([A-Za-z_][A-Za-z0-9_]*)$/.exec(path);
  if (!parts) continue;
  const signature = overloads[0];
  const members = moduleShapes.get(parts[1]) ?? [];
  members.push({ name: parts[2], type: `fn(${signature.parameters.map((p) => p.type).join(", ")}): ${signature.returns}`, kind: "method", signature });
  moduleShapes.set(parts[1], members);
}
moduleShapes.get("http")!.push({ name: "status", type: "HttpStatus", kind: "field" }, { name: "code", type: "HttpCode", kind: "field" });
/** Direct module exports only; receiver methods never leak into a module. */
export function moduleMembers(path: string): readonly ApiMember[] { return moduleShapes.get(path.replace(/^std\./, "")) ?? []; }

const aliases: Readonly<Record<string, string>> = { pg_client: "PgClient", pg_result: "PgResult", HttpServer: "HttpService", time: "Time", date: "Date", duration: "Duration" };
function canonicalPath(path: string): string {
  const [receiver, ...rest] = path.replace(/^std\./, "").split(".");
  // Lowercase time is a module, not the Time value type.
  return `${receiver === "pg_client" || receiver === "pg_result" ? aliases[receiver] : receiver}${rest.length ? `.${rest.join(".")}` : ""}`;
}
export function stdSignatures(path: string): readonly ApiSignature[] { return signatures.get(canonicalPath(path)) ?? []; }
export function stdSignature(path: string, argumentCount?: number): ApiSignature | undefined {
  const overloads = stdSignatures(path);
  return argumentCount === undefined ? overloads[0] : overloads.find((signature) => argumentCount <= signature.parameters.length
    && argumentCount >= signature.parameters.filter((parameter) => !parameter.optional).length);
}
export function typeMembers(type: string): readonly ApiMember[] {
  const array = /^\[(.*)\]$/.exec(type.trim());
  const name = array ? "Array" : aliases[type] ?? type;
  const members = shapes.get(name) ?? [];
  if (!array) return members;
  const substitute = (text: string) => text.replace(/\bT\b/g, array[1]);
  return members.map((member) => ({ ...member, type: substitute(member.type), ...(member.signature ? {
    signature: { ...member.signature, label: substitute(member.signature.label), returns: substitute(member.signature.returns),
      parameters: member.signature.parameters.map((parameter) => ({ ...parameter, type: substitute(parameter.type) })) },
  } : {}) }));
}

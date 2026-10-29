---
title: HTTP reference
description: Build an HTTP service, call endpoints, register routes, and handle request and response limits.
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'src/stdlib/http.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'src/backend/c_http_runtime.c',
    'src/backend/c_task_http.rs',
    'src/ir/task_lower.rs',
    'src/service_supervision.rs',
    'src/ir/mod.rs',
    'src/backend/c.rs',
    'tests/native_http_test.rs',
  ]
sidebar:
  order: 8
---

`std.http` provides an HTTP client, route registration, and complete response values. Save the server and client below in separate files.

The first server and client examples use **the interpreter (`ku run`) or the default runner**. Native C supports synchronous JSON responses but not the module-level HTTP client. Use the [native C example](#native-c) or the [named async handler example](#named-async-native-handlers) for native programs.

## Start a server

Save as `server.ku`, then run `ku run server.ku`. Port 8080 must be available. Stop the server with Ctrl+C.

```ku
import http from "std.http"

fn main(): null! {
    app = http.service({ max_body_bytes: 65536, handler_timeout_ms: 5000 })
    app.get("/health", fn() { return http.text("ok") })
    app.get("/users/{id}", fn(req) {
        return http.json({
            id: req.params.id.clone(),
            q: req.query.get_or("q", ""),
            method: req.method.clone()
        })
    })
    app.post("/echo", fn(req) { return http.text(req.body.clone()) })
    app.get("/empty", fn() { return http.empty() })
    app.get("/page", fn() { return http.html("<h1>Ku HTTP</h1>") })
    app.get("/go", fn() { return http.redirect("/health") })
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

With the server running, try:

```sh
curl 'http://127.0.0.1:8080/health'
curl 'http://127.0.0.1:8080/users/42?q=ku'
curl -X POST --data 'hello' 'http://127.0.0.1:8080/echo'
```

These return `ok`, `{"id":"42","method":"GET","q":"ku"}`, and `hello`. `GET /empty` returns 204 without a body, `/page` returns HTML, and `/go` redirects to `/health`.

Request strings belong to the request. The explicit `.clone()` calls give the response independent ownership of the fields used in its body; they are not needed merely to inspect a field.

## Call the server

Save as `client.ku`, keep the server running, and run `ku run client.ku`:

```ku
import http from "std.http"

fn main(): null! {
    response = http.get("http://127.0.0.1:8080/users/42?q=ku")?
    println(response.status)
    println(response.body)

    echo = http.request({
        method: "POST",
        url: "http://127.0.0.1:8080/echo",
        headers: { "Content-Type": "text/plain; charset=utf-8" },
        body: "你好 Ku",
        timeout_ms: 1000,
        max_body_bytes: 65536
    })?
    println(echo.body)

    missing = http.get("http://127.0.0.1:8080/missing")?
    println(missing.status)
    return ok(null)
}
```

Output:

```text
200
{"id":"42","method":"GET","q":"ku"}
你好 Ku
404
```

An HTTP 404 or 500 is an `Ok(HttpResponse)`, not a failed Result. Check `response.status`. Connection, URL, timeout, body-size, and UTF-8 failures return errors. Use `?` in a Result-returning function or `try/catch` as described in [error handling](/en/docs/guide/errors/).

## Client interfaces

The names below describe client signatures and response values. The dedicated [native async handler path](#named-async-native-handlers) also accepts `HttpRequest` and `HttpResponse` annotations; that does not make the module-level HTTP client available in native code.

| Call                           | Return          | Description                                                                    |
| ------------------------------ | --------------- | ------------------------------------------------------------------------------ |
| `http.get(url:str)`            | `HttpResponse!` | GET using the shared default client                                            |
| `http.post(url:str, body:str)` | `HttpResponse!` | POST a UTF-8 string                                                            |
| `http.request(options)`        | `HttpResponse!` | GET or POST with per-request options                                           |
| `http.client(options?)`        | object          | Construct a configuration value; it has no `get/post/request` receiver methods |

A response has `status:int`, `headers:object` with string values, and `body:str`. Received header names are lowercased. The client accepts absolute `http://` or `https://` URLs. `http.request` currently supports **GET and POST only**; server PUT/DELETE routes do not add client methods.

| Request option   | Type              | Default / rule                                           |
| ---------------- | ----------------- | -------------------------------------------------------- |
| `url`            | str               | Required                                                 |
| `method`         | str               | `"GET"`; GET or POST, case insensitive                   |
| `headers`        | object of strings | Empty; field names and values must be valid HTTP headers |
| `body`           | str               | No request body by default                               |
| `timeout_ms`     | int               | 5,000; 1–300,000                                         |
| `max_body_bytes` | int               | 1,000,000; 1–16,777,216; response body limit             |

The default client reuses connections. The configuration object from `http.client` accepts `timeout_ms`, `max_body_bytes`, and `max_idle_connections` (default 128, range 1–1,024), but it is **not attached to the module-level request calls**. Use the per-request fields above for an effective timeout or response-size change. Unknown configuration fields and camelCase spellings are rejected.

Client errors use domain `http`; codes include `invalid_method`, `invalid_url`, `transport`, `read_failed`, `body_too_large`, and `invalid_utf8`. Invalid option types or malformed headers may be rejected by checking or as a runtime error before a request is sent.

## Routes and request fields

| Service method                | Result    | Meaning                                                 |
| ----------------------------- | --------- | ------------------------------------------------------- |
| `app.get(path:str, handler)`  | null      | Register GET                                            |
| `app.post(path:str, handler)` | null      | Register POST                                           |
| `app.put(path:str, handler)`  | null      | Register PUT                                            |
| `app.del(path:str, handler)`  | null      | Register DELETE; this does not remove a route           |
| `app.listen(address:str)`     | `null!`   | Consume the service and enter its blocking request loop |
| `app.bind(address:str)`       | listener! | Bind without running; interpreter only                  |
| `listener.run()`              | `null!`   | Enter the blocking loop; interpreter only               |
| `listener.close()`            | `null!`   | Close an unstarted listener; interpreter only           |

`http.service(options?)` and `http.server(options?)` construct equivalent service values. Register routes before starting the service. Use `/users/{id}` for path parameters. `:id`, duplicate route shapes, and ambiguous or invalid patterns are rejected. A handler accepts zero parameters (`fn()`) or one (`fn(req)`), and returns `http.text/json/html/empty/redirect(...)` or a Result containing that response. It cannot mutate captured outer variables or rebind function bindings shared by registered handlers. There is no response writer argument.

| Request field | Type              | Meaning                          |
| ------------- | ----------------- | -------------------------------- |
| `req.method`  | str               | HTTP method                      |
| `req.path`    | str               | Request path                     |
| `req.params`  | object of strings | Named path captures              |
| `req.query`   | object of strings | Parsed query fields              |
| `req.headers` | object of strings | Request headers; lowercase names |
| `req.body`    | str               | UTF-8 request body               |

Use `req.query.get_or("q", "")` for an optional field in interpreted code. Ordinary missing-key reads fail in the interpreter; native HTTP request-map field reads currently return an empty string for a missing key. Do not rely on a missing required field being rejected consistently across these paths. Inline `fn(req)` gives the checker the request-field context. The interpreter and default runner do not expose a nominal `HttpRequest` parameter type for ordinary handlers; native async handler annotations are described below.

## Responses and status codes

| Helper                                                                    | Default status | Content type / behavior                             |
| ------------------------------------------------------------------------- | -------------- | --------------------------------------------------- |
| `http.text(body:str)` / `http.text(status:int, body:str)`                 | 200            | `text/plain; charset=utf-8`                         |
| `http.html(body:str)` / `http.html(status:int, body:str)`                 | 200            | `text/html; charset=utf-8`                          |
| `http.json(value)` / `http.json(status:int, value)`                       | 200            | Compact JSON, `application/json; charset=utf-8`     |
| `http.empty(status?:int)`                                                 | 204            | Empty body                                          |
| `http.redirect(location:str)` / `http.redirect(status:int, location:str)` | 302            | `location` header                                   |
| `http.statusText(code:int)`                                               | —              | Standard reason text, such as `"Not Found"` for 404 |

Statuses must be 100–599. Redirect helpers accept 301, 302, 303, 307, or 308. `http.json` accepts the same representable data as [JSON encoding](/en/docs/stdlib/json/); it does not return a Result and cannot recoverably encode unsupported values. These helpers return complete response objects with `status`, `headers`, and `body`. A protocol status is separate from any application field such as `body.code`.

`http.status` contains:

```text
ok=200, created=201, accepted=202, noContent=204
movedPermanently=301, found=302, seeOther=303, notModified=304
temporaryRedirect=307, permanentRedirect=308
badRequest=400, unauthorized=401, forbidden=403, notFound=404
methodNotAllowed=405, notAcceptable=406, requestTimeout=408
conflict=409, gone=410, contentTooLarge=413, uriTooLong=414
unsupportedMedia=415, rangeNotSatisfiable=416, unprocessable=422
tooManyRequests=429, headerTooLarge=431
internalError=500, notImplemented=501, badGateway=502
serviceUnavailable=503, gatewayTimeout=504
```

`http.code` is a legacy uppercase alias object; use the descriptive `http.status` names in new code.

## Server limits and deadlines

Supply options at construction, for example `http.service({ max_body_bytes: 65536 })`. Existing public option fields may also be assigned before binding. All numeric options must be positive integers; zero does not disable a limit.

| Option                   |    Default |       Maximum | Applies to                                                       |
| ------------------------ | ---------: | ------------: | ---------------------------------------------------------------- |
| `read_header_timeout_ms` |      5,000 |       300,000 | Receiving request headers                                        |
| `read_body_timeout_ms`   |     10,000 |       300,000 | Receiving a request body                                         |
| `write_timeout_ms`       |     10,000 |       300,000 | Sending a response                                               |
| `idle_timeout_ms`        |      5,000 |       300,000 | Waiting for a request's first byte, including reused connections |
| `handler_timeout_ms`     |     15,000 |       300,000 | Handler deadline; see start point below                          |
| `max_body_bytes`         |  1,000,000 |    16,777,216 | Request body                                                     |
| `max_header_bytes`       |     16,384 |        65,536 | Request headers                                                  |
| `max_connections`        |      1,024 |        32,768 | Open connections                                                 |
| `max_active_requests`    |        256 |        16,384 | Active request slots                                             |
| `max_pending_requests`   |      1,024 |         8,192 | Waiting requests                                                 |
| `max_buffer_bytes`       | 67,108,864 | 1,073,741,824 | Per-listener controlled transport buffers                        |

The interpreter starts `handler_timeout_ms` when a complete request becomes ready, including its wait for execution. Native C starts the handler phase budget before dispatch; the native runtime also has an outer request deadline from its first observation of a request byte. `KU_HTTP_REQUEST_TIMEOUT_MS` configures that native outer budget (default 30,000 ms, maximum 300,000 ms); it is an environment setting, not an `http.service` field. The effective async handler/PG deadline is clamped to the original request deadline and is not renewed by admission, queueing or connection acquisition. First-byte observation is not a packet-arrival timestamp or a client end-to-end clock.

The native runtime additionally limits handler workers to 64. The transport buffer budget does not bound the entire process, user heap, thread stacks, kernel socket buffers, or external libraries.

The runtime returns 400 for malformed requests, 404 for an unknown route, 405 for an unsupported method on an existing path, 413 for an oversized body, 414 for a target over 8,192 bytes, 431 for oversized headers, 503 for admission/buffer exhaustion, and 504 for a handler deadline. A native deadline is cooperative: loops and Ku call returns are checked; it cannot forcibly interrupt an arbitrary blocking foreign call. Increasing the deadline is not a fix for a stalled handler.

HTTP/1.1 connections can be reused, with ordered pipelined responses. A request containing `Connection: close`, a 4xx/5xx response, a protocol error, or an expired idle deadline closes the connection. Streaming responses, SSE, WebSocket and a public `app.stop()` method are not implemented. Supervised native services can use authenticated health/drain control through `ku service`; this is separate from listener `close()`. See [supported execution paths and known issues](/en/docs/support/) before choosing a deployment path.

## Bind without starting

This complete interpreter example requests a free port and releases it without serving requests:

```ku
import http from "std.http"

fn main(): null! {
    app = http.service()
    listener = app.bind("127.0.0.1:0")?
    println(listener.address)
    listener.close()?
    return ok(null)
}
```

The listener holds the bound socket until it is run, explicitly closed, or its last owning value leaves scope. It cannot be restarted after `run` or `close`. `close` is not a concurrent shutdown handle for an already running server.

<span id="native-c"></span>

## Execution support

`ku run` and the default packaged runner support the complete server/client examples above. Synchronous native C supports `app.listen(...)` with `text/html/json/empty/redirect` responses. `http.json(value)` and `http.json(status, value)` use the synchronous JSON writer and set `application/json; charset=utf-8`; they return a response, not a Result. Invalid status or unsupported encoding fails the handler (500), rather than becoming an ordinary recoverable Result. Native C does not implement `http.statusText`, the module-level HTTP client, or `bind/run/close`. Named async native handlers use the restricted path below; it does not provide the complete interface above. LLVM does not provide this HTTP library.

For a server that can also be compiled to native C, save the following as `native-server.ku`. `/users/42` returns the plain text `42` in this version:

```ku
import http from "std.http"

fn main(): null! {
    app = http.service()
    app.get("/health", fn() { return http.text("ok") })
    app.get("/users/{id}", fn(req) {
        return http.text(req.params.id.clone())
    })
    app.post("/echo", fn(req) { return http.text(req.body.clone()) })
    app.get("/empty", fn() { return http.empty() })
    app.get("/page", fn() { return http.html("<h1>Ku HTTP</h1>") })
    app.get("/go", fn() { return http.redirect("/health") })
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

```sh
ku build --native native-server.ku -o native-server
./native-server
```

On Windows choose `-o native-server.exe` and run `.\native-server.exe`. Use the earlier curl commands to call it, then stop it with Ctrl+C. Do not run both examples on port 8080 simultaneously.

## Named async native handlers

This separate native example uses a named async handler. Save it as `async-server.ku`; check and compile it with the native C backend, rather than interpreting it:

```ku
import http from "std.http"

fn main(): null! {
    async fn health(): HttpResponse! {
        return ok(http.text("ready"))
    }
    app = http.service({
        max_connections: 64,
        max_active_requests: 16,
        max_pending_requests: 32
    })
    app.get("/health", health)
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

```sh
ku check async-server.ku
ku build --backend c --profile release async-server.ku -o async-server
./async-server
```

`curl http://127.0.0.1:8080/health` returns `ready`. Stop the binary with Ctrl+C before running another example on the same port. On Windows use an `.exe` output name.

The supported handler annotations are `HttpRequest` and `HttpResponse` in this native HTTP path. A handler returning `HttpResponse!` uses Result propagation; the runtime owns admission, its Task and the response lifetime. This does not make arbitrary resource handles, dynamic closures or every standard-library operation valid in a Task. The interpreter rejects async HTTP handlers. Within these async handlers, supported response constructors are `text/html/empty/redirect`; use the [PG example](/en/docs/stdlib/database/#async-pg) for the limited read-only client capture path.

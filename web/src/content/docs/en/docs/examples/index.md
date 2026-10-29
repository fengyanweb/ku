---
title: 'Examples'
description: 'Find runnable Ku examples by task.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources: ['examples/README.md', 'examples/http_server.ku', 'examples/json.ku']
---

Examples are organized by task. Each linked page supplies a complete program, execution instructions and backend requirements. File, server and database prerequisites are stated explicitly.

| Task                        | Example and environment                                                          |
| --------------------------- | -------------------------------------------------------------------------------- |
| First program               | [Hello Ku](/en/docs/getting-started/); interpreter or native C                   |
| Chinese and emoji           | [Strings](/en/docs/stdlib/string/); interpreter or listed native methods         |
| JSON round trips and errors | [JSON](/en/docs/stdlib/json/); interpreter or synchronous native C               |
| Write and read a file       | [Filesystem](/en/docs/stdlib/fs/); creates a file beside the source              |
| Dates and waiting           | [Time](/en/docs/stdlib/time/); wall-clock versus monotonic time                  |
| HTTP server and requests    | [HTTP](/en/docs/stdlib/http/); start the local server first                      |
| Tasks and await             | [Async calls](/en/docs/guide/concurrency/); one-use handles and explicit Results |
| Multi-file project          | [Packages and modules](/en/docs/guide/packages/); local imports                  |
| Database queries            | [PG / Redis / MySQL](/en/docs/stdlib/database/); native C and real services      |
| Binary data and TCP         | [bytes](/en/docs/stdlib/bytes/) / [net](/en/docs/stdlib/net/); native C          |

The repository's [examples directory](https://github.com/fengyanweb/ku/tree/v0.0.19/examples) contains larger examples.

## Native Task examples

| Complete program                                                                    | Requirement and expected result                                               |
| ----------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| `async-server.ku` in [HTTP](/en/docs/stdlib/http/#named-async-native-handlers)      | Native C; free port 8080; `GET /health` returns `ready`                       |
| `task-json.ku` in [JSON](/en/docs/stdlib/json/#scalar-json-objects-in-native-tasks) | Native Task scalar object encoding; deterministic JSON text                   |
| `task-sleep.ku` in [time](/en/docs/stdlib/time/#native-task-wait)                   | Integer timer wait; prints `awake`; no external service                       |
| `async-pg.ku` in [database clients](/en/docs/stdlib/database/#async-pg)             | Native C, libpq and a real private PG configuration; `GET /read` returns `Ku` |

These programs demonstrate different supported paths. They do not establish a performance limit, a full async library or production readiness.

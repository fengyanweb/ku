---
title: 'Examples'
description: 'Find runnable Ku examples by task.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['examples/README.md', 'examples/http_server.ku', 'examples/json.ku']
slug: en/v0.0.18/docs/examples
---

Examples are organized by task. Each linked page supplies a complete program, execution instructions and backend requirements. File, server and database prerequisites are stated explicitly.

| Task                        | Example and environment                                                          |
| --------------------------- | -------------------------------------------------------------------------------- |
| First program               | [Hello Ku](/en/v0.0.18/docs/getting-started/); interpreter or native C                   |
| Chinese and emoji           | [Strings](/en/v0.0.18/docs/stdlib/string/); interpreter or listed native methods         |
| JSON round trips and errors | [JSON](/en/v0.0.18/docs/stdlib/json/); interpreter or synchronous native C               |
| Write and read a file       | [Filesystem](/en/v0.0.18/docs/stdlib/fs/); creates a file beside the source              |
| Dates and waiting           | [Time](/en/v0.0.18/docs/stdlib/time/); wall-clock versus monotonic time                  |
| HTTP server and requests    | [HTTP](/en/v0.0.18/docs/stdlib/http/); start the local server first                      |
| Tasks and await             | [Async calls](/en/v0.0.18/docs/guide/concurrency/); one-use handles and explicit Results |
| Multi-file project          | [Packages and modules](/en/v0.0.18/docs/guide/packages/); local imports                  |
| Database queries            | [PG / Redis / MySQL](/en/v0.0.18/docs/stdlib/database/); native C and real services      |
| Binary data and TCP         | [bytes](/en/v0.0.18/docs/stdlib/bytes/) / [net](/en/v0.0.18/docs/stdlib/net/); native C          |

The repository's [examples directory](https://github.com/fengyanweb/ku/tree/v0.0.18-r1/examples) contains larger examples.

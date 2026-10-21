---
title: Configuration files
description: Load .env and flat YAML configuration from source-relative files with std.config.
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/config.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 7
---

Import with `import config from "std.config"`. This module loads files; `config.env()` does **not** read or merge the process environment.

## Load a small configuration

Create these three files beside `config.ku`:

`.env`:

```dotenv
APP_NAME=Ku
```

`extra.env`:

```dotenv
MODE="development"
```

`app.yaml`:

```yaml
port: 8080
debug: true
```

Save the following program as `config.ku`, then run `ku run config.ku`:

```ku
import config from "std.config"

fn main(): null! {
    env = config.env()
    println(env.APP_NAME)
    extra = config.env_file("extra.env")
    println(extra.MODE)

    settings = config.yaml("app.yaml")?
    println(settings.port)
    println(settings.debug)
    return ok(null)
}
```

Output:

```text
Ku
development
8080
true
```

Keep real credentials out of source control, public examples, and logs. All values shown here are demonstration data.

## Interface and path resolution

| Call                        | Return            | Missing file / failure                                                                                        |
| --------------------------- | ----------------- | ------------------------------------------------------------------------------------------------------------- |
| `config.env()`              | object of strings | Read `.env` beside the current source; missing file returns an empty object; read/parse errors stop execution |
| `config.env_file(path:str)` | object of strings | Read an explicit file; read/parse errors stop execution                                                       |
| `config.yaml(path:str)`     | `object!`         | Read flat YAML; return a recoverable error on failure                                                         |

Relative paths are resolved from the current `.ku` source directory; absolute paths are accepted. Files are UTF-8 and limited to 1,000,000 bytes. Returned objects own their contents; they do not hold an open configuration-file handle. Repeated calls read the file again. There is no automatic reload, watcher, environment interpolation, or merge precedence.

Use `get_or` when a field is optional, for example `env.get_or("APP_NAME", "Ku")`; directly reading a missing field is an error. A missing default `.env` therefore does not make `env.APP_NAME` valid.

## .env format

- One `KEY=value` entry per line, split at the first `=`.
- Keys start with an ASCII letter or `_`, followed by letters, digits, or `_`.
- Empty lines and lines starting with `#` are ignored.
- Surrounding whitespace is trimmed; single- and double-quoted values are accepted.
- Quoted values support `\n`, `\r`, `\t`, `\\`, `\"`, and `\'` escapes.
- Values are strings, including `true`, `8080`, and empty values. Later duplicate keys replace earlier ones.

`export KEY=value`, multiline values, `${NAME}` interpolation, and inline comment syntax are not implemented. A trailing `# text` is part of the value, not a comment.

## Flat YAML format

Only unindented `key: value` entries are supported. Keys use the same ASCII identifier rules as `.env`. Empty lines and full-line `#` comments are ignored. Values are `null`, lowercase `true/false`, signed integers, floats, quoted strings, or unquoted strings. Later duplicate keys replace earlier ones.

This is a small flat-configuration reader, not a complete YAML implementation. Nested mappings, arrays, multiline scalars, tags, and anchors are not supported. Do not use inline comments; they can become part of an unquoted string. For richer nested data use [JSON](/en/docs/stdlib/json/) together with [file reading](/en/docs/stdlib/fs/).

## Errors and execution support

`config.yaml` errors use `{ domain: "config", code: "read_failed", message: ... }`, including parse failures. `env` and `env_file` do not return Result; use `yaml` where recoverable file/parse errors are required. `?` is only appropriate on `yaml`, not `env/env_file`.

The module is implemented for `ku run` and the default packaged runner. Synchronous native C, native Task, and LLVM do not provide these configuration APIs. Async interpreted calls can use the existing blocking-worker path; that does not change file syntax or return types.

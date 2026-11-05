---
title: 'Packages and modules'
description: 'Organize source, import libraries and manage dependencies.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources: ['docs/package.md', 'src/package.rs']
sidebar:
  order: 3
---

The standard library ships with Ku. Use imports for your own modules and third-party packages. There is no officially hosted public package directory; the repository provides a self-hosted registry implementation.

## Create or initialize a project

```sh
ku create hello
cd hello
ku check
ku run
```

The default basic template prints `Hello Ku`. Choose HTTP with `ku create my-api -t http`; `-t` equals `--template`. Initialize an existing empty directory with `ku init -t cli`; it refuses to overwrite an existing `ku.mod` or `src/main.ku`. `ku template list` and `ku create --list` list the same built-in templates: basic, cli, http, json, fs and lib.

The layout below shows handwritten modules. A valid `ku.mod` selects the entry through `root` / `main`; Ku does not choose an arbitrary same-named file. See the [CLI reference](/en/docs/tools/cli/) for complete options.

## Project layout

```text
hello/
  ku.mod
  src/
    main.ku
    util.ku
```

`ku.mod`:

```toml
name = "hello"
version = "0.1.0"
root = "src"
main = "main.ku"
```

`src/util.ku`:

```ku
fn Greeting(): str {
    return "Hello Ku"
}
```

`src/main.ku`:

```ku
import { Greeting } from "util"

fn main() {
    println(Greeting())
}
```

Run `ku check` and `ku run` from the project directory. The output is `Hello Ku`. Exported user names start with an ASCII uppercase letter; lowercase names remain private to their file.

## Import paths

| Syntax           | Resolution                                   |
| ---------------- | -------------------------------------------- |
| `"std.json"`     | Bundled standard library                     |
| `"util"`         | Current package's root directory             |
| `"./util.ku"`    | Relative to the current source file          |
| `"@math/vector"` | A module inside the declared math dependency |

Local development dependencies use `dep.name.source = "file://absolute/path"`. Cross-package dependencies must be declared in `ku.mod`; imports cannot escape package roots.

## Registries and locks

Registry dependencies need an explicit HTTPS URL and pinned Ed25519 public key. `ku package resolve` updates resolution and the lock; `--locked` uses the existing lock and `--offline` uses cached packages only. `ku package pack` creates an artifact. `publish` and `yank` write to the configured registry and require credentials.

See the repository's [package reference](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/package.md) for all fields, version ranges and self-hosted setup.

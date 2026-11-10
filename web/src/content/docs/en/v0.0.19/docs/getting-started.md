---
title: Getting started
description: Write Hello Ku, check your source, and distinguish interpretation from compilation.
kuVersion: '0.0.19'
verifiedAt: '2026-10-30'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.19.md']
---

[Install Ku](/en/v0.0.19/docs/install/) first, then run `ku -v` (equivalent to `ku --version`) and confirm it reports `ku 0.0.19`.

## Write your first program

Create `hello.ku` with the same contents as the repository's `examples/hello.ku`:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` is the program entry point, and `println` writes one line of text. This program needs no return type annotation, `!`, `?`, or `return ok(null)`. The entry function itself is still required. Ku does not use `let` or `let mut`.

Operations such as reading a file or parsing JSON may return a Result. Use [`T!` and `?` as explained in error handling](/en/v0.0.19/docs/guide/errors/#result-operators) when working with those results.

## Check and run

From the file's directory:

```sh
ku check hello.ku
ku ./hello.ku
```

`check` checks lexical, syntax, and basic semantic errors without running the program. `ku ./hello.ku` is equivalent to `ku run ./hello.ku`; Windows also accepts `ku .\hello.ku`. This shorthand accepts only one source-file path. The expected standard output when running is:

```text
Hello Ku
```

If Ku is not on `PATH`, invoke the executable using its full installation path. The Windows executable is `ku.exe`.

## Variables and functions

This basic syntax fragment can be placed inside `main`:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

The first assignment declares a variable. An explicit type uses `name:type = value`. `print` writes without an automatic newline; `println` adds one.

## Build an executable

The installation page lists the required build tools. For the default backend:

```sh
ku build hello.ku
```

The default backend packages an interpreter runner. Native C compilation requires a separate C compiler:

```sh
ku build --backend c hello.ku
```

Default output lives under `.ku/build/<profile>/`; use the precise path reported by the CLI. The two backends cover different features. Passing `ku check` does not prove a native build will succeed.

Note: `ku build --native hello.ku` **without `-o` emits C source only**, without linking. Use the `--backend c` command above for your first compiled program.

## Create a project

Use a built in template when you need a package directory:

```sh
ku create HelloWorld -t basic
cd HelloWorld
ku check
ku run
```

`-t` is short for `--template`; use `ku init -t basic` to initialize the current empty directory. The package entry is configured in `ku.mod`. `ku create --list` lists available templates. There are currently no `ku test` or `ku fmt` commands.

Continue with the [language guide](/en/v0.0.19/docs/guide/language/), [troubleshooting](/en/v0.0.19/docs/guide/errors/), and [backend support](/en/v0.0.19/docs/support/).

See the [CLI reference](/en/v0.0.19/docs/tools/cli/) for full commands and their shorthands.

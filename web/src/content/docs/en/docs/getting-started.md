---
title: Getting started
description: Write Hello Ku, check your source, and distinguish interpretation from compilation.
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.19.md']
---

[Install Ku](/en/docs/install/) first and confirm that `ku --version` runs.

## Write your first program

Create `hello.ku` with the same contents as the repository's `examples/hello.ku`:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` is the program entry point, and `println` writes one line of text. This program needs no return type annotation, `!`, `?`, or `return ok(null)`. The entry function itself is still required. Ku does not use `let` or `let mut`.

Operations such as reading a file or parsing JSON may return a Result. Use [`T!` and `?` as explained in error handling](/en/docs/guide/errors/#result-operators) when working with those results.

## Check and run

From the file's directory:

```sh
ku check hello.ku
ku run hello.ku
```

`check` checks lexical, syntax, and basic semantic errors without running the program. The expected standard output when running is:

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
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

The package entry is configured in `ku.mod`. `ku create --list` lists available templates. There are currently no `ku test` or `ku fmt` commands.

Continue with the [language guide](/en/docs/guide/language/), [troubleshooting](/en/docs/guide/errors/), and [backend support](/en/docs/support/). The outputs here are expected examples and do not replace installation verification on your platform.

## Where to go next

| Goal                                    | Read next                                                           | Execution requirement                                                 |
| --------------------------------------- | ------------------------------------------------------------------- | --------------------------------------------------------------------- |
| Understand values, ownership and errors | [Language guide](/en/docs/guide/language/)                          | Start with `check` and `run`                                          |
| Encode data and inspect dynamic fields  | [JSON](/en/docs/stdlib/json/)                                       | General synchronous API or the explicitly limited native Task example |
| Serve HTTP                              | [HTTP](/en/docs/stdlib/http/)                                       | Synchronous interpreter example or separately compiled native handler |
| Wait or measure elapsed work            | [Time](/en/docs/stdlib/time/) / [Task](/en/docs/guide/concurrency/) | Choose clocks and waits supported by your backend                     |
| Query a database                        | [Database clients](/en/docs/stdlib/database/)                       | Native C, native libraries, a real service and private credentials    |
| Run a resident service with limits      | [CLI](/en/docs/tools/cli/) / [support](/en/docs/support/)           | Configure process supervision and OS prerequisites explicitly         |

Examples identify their backend and external dependencies. Start with one complete program; running every example directory as a batch would also start servers and contact databases.

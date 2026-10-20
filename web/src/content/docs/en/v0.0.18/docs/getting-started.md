---
title: Getting started
description: Write Hello Ku, check your source, and distinguish interpretation
  from compilation.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - examples/hello.ku
  - README.md
  - docs/syntax.md
  - docs/v0.0.18.md
slug: en/v0.0.18/docs/getting-started
---

This documentation describes the frozen scope of the v0.0.18 experimental prerelease. Actual publication and downloadable assets are recorded in GitHub Releases. This page uses basic syntax and CLI commands already present in v0.0.17. [Install Ku](/en/v0.0.18/docs/install/) first and confirm that `ku --version` runs.

## Write your first program

Create `hello.ku` with the same contents as the repository's `examples/hello.ku`:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` is the entry point. `println` writes one line of text. Ku does not use `let` or `let mut`.

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

Continue with the [language guide](/en/v0.0.18/docs/guide/language/), [troubleshooting](/en/v0.0.18/docs/guide/errors/), and [backend support](/en/v0.0.18/docs/support/). The outputs here are expected examples and do not replace installation verification on your platform.

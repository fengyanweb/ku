# Ku

[简体中文](README.md) · [Documentation source](web/src/content/docs/en/docs)

Ku is an experimental programming language with an interpreter, static checking, native C compilation, a standard library, and a VS Code extension. The current source version is **0.0.19**, licensed under [Apache-2.0](LICENSE); see [NOTICE](NOTICE) for attribution.

This version brings together multi-loop native HTTP, supported native Task paths, asynchronous PostgreSQL integration, service recovery, and verified runtime fixes. Controlled deployment rehearsals do not establish production readiness. Long-tail latency remains a known issue under some loads. Read the [release notes](docs/v0.0.19.en.md) and [known limitations](docs/v0.0.19-known-issues.en.md).

## Install and run

Choose the matching OS and architecture from [GitHub Releases](https://github.com/fengyanweb/ku/releases), verify the supplied `SHA256SUMS`, extract the archive, and add its directory to `PATH`. Keep the accompanying libraries and metadata with `ku` (`ku.exe` on Windows).

```sh
ku version
ku create HelloWorld
cd HelloWorld
ku check
ku run
```

```ku
fn main() {
    println("Hello, Ku!")
}
```

From this repository:

```sh
ku run examples/hello.ku
ku check examples/json.ku
ku build --backend c --profile release examples/hello.ku -o hello
```

Backend coverage differs. Database drivers need native output and their client libraries. Multi-loop HTTP and owner Task execution refer to the native C backend; a successful `ku check` does not prove that every backend supports a program.

## Documentation

- [Guides and library reference](web/src/content/docs/en/docs): language syntax, ownership, errors, HTTP, Task, JSON, time, databases, and platform limits.
- [Runnable examples](examples), [syntax contract](docs/syntax.md), and [semantic contract](docs/semantics.md).
- [Task semantics and backend coverage](docs/concurrency.md).
- [VS Code extension](editors/vscode-ku/README.md).
- [Current seven-language fixed-load comparison](docs/performance-v0.0.19.en.md).
- [Release notes](docs/v0.0.19.en.md) and [known limitations](docs/v0.0.19-known-issues.en.md).

Use `ku --help` and subcommand help for the installed version's options. Common commands include `check`, `run`, `build`, `ir`, `package`, `service`, and `supervise`.

## Build from source

Rust 1.89.0 is required. Native C output also needs a supported C compiler and any native libraries used by the program.

```sh
cargo +1.89.0 build --locked --release
./target/release/ku version
./target/release/ku run examples/hello.ku
```

On Windows, use `target\release\ku.exe`. Existing regression tests and CI fixtures are maintained source; disposable probes and duplicate build artifacts should not be committed.

## History

[0.0.18](docs/v0.0.18.md), its [historical revision](docs/v0.0.18-r1.md), [0.0.17](docs/v0.0.17.md), and [0.0.16](docs/v0.0.16.md) remain available with their original release identities. Historical performance results apply only to their recorded commit, workload, platform, and limits. Updated website source does not imply an online deployment.

# Ku Programming Language for VS Code

[中文说明](README.zh-CN.md)

Extension **0.0.21** (`ku-lang.ku-language`) supports the current Ku language workflow: write and run a file, edit structured asynchronous HTTP/PG code, and work with installed, locked third-party packages. Extension versions are independent of CLI versions. This delivery was checked against the daily **Ku 0.0.20** SDK whose source identity starts with `264e53d44f21`; another SDK with the same version number may expose different capabilities.

## Install and choose Ku

Install `ku-language-0.0.21.vsix` with **Extensions → … → Install from VSIX…**, then reload the window when VS Code requests it. A CLI alternative is:

```sh
code --install-extension /absolute/path/ku-language-0.0.21.vsix --force
```

Run **Ku：选择可执行文件** to pick an installed `ku` or Windows `ku.exe`. The status bar and **Ku: Show Version** show the actual executable path, CLI version and extension version. The extension does not install or replace an SDK or alter PATH.

The existing resource setting `ku.executablePath` is preserved. It accepts Chinese/space-containing paths, `${workspaceFolder}`, `${userHome}`, `~/` and workspace-relative paths without shell quoting or arguments. An invalid explicit path is reported, without falling back to another SDK. When empty, discovery checks the extension host PATH and the containing workspace's release/target locations. Remote windows require a remote CLI. Static editing remains available without a CLI and in Restricted Mode; tool execution requires a trusted workspace.

## What the editor understands

- Local lexical scopes, parameters, declarations, closures and aliases. Go to Definition respects function/block boundaries.
- Structs and literal object fields; known function return types, async Task and Result unwrapping; nested destructuring with renamed/default/rest bindings and optional `?.` access. Unknown or conflicting types do not receive invented fields.
- Signature help and hover metadata for the delivered time Date/Time/Duration overloads, non-consuming array concat/first/last, HTTP service/client/request/response, ordinary and typed PG, and asynchronous PG calls. A PG client's query inside an async function is the existing async runtime path; it is not a new `query_async` API.
- Highlighting/snippets for structured asynchronous code, Result, nullable fields and rest patterns. Completion in comments and ordinary strings is suppressed, with import paths handled separately.
- Formatting preserves strings, multiline literals, comment text, CRLF and indentation options. It leaves unfinished literals alone. Existing structured HTTP and async HTTP/PG examples were checked before/after formatting and for a second identical formatting pass.

The model is deliberately conservative and local, rather than a complete language server. It caches document parsing by document version and uses a compact audited metadata table, not an SDK scan on every open. Runtime/compiler errors remain authoritative. Dynamic JSON and arbitrary control-flow/type inference have limits; see [VERIFICATION.md](VERIFICATION.md).

## Run, check, build and tools

Existing command IDs and user check settings are preserved. Run uses an interactive integrated process terminal, targeting the active editor or the file clicked in Explorer, and saves dirty content first. Cancelling or failing a save stops the command. In a multi-root workspace commands use the containing Ku project; loose files use their directory.

| Command | CLI behavior |
| --- | --- |
| Ku Run | `ku run FILE`; interactive stdin belongs to this terminal. |
| Check Current File | `ku check --json FILE`; Problems shows compiler diagnostics, Ku Output shows command/cwd/errors. |
| Build Current File | `ku build FILE`; follows the chosen CLI's default backend. The tested daily SDK defaults to native C. |
| Ku：构建选项 | One picker for supported default/Release/locked/Runner choices; Runner requires its matching toolchain doctor check. |
| Show IR | `ku ir FILE`. |
| Emit Native C Source | Existing `ku build --native FILE` compatibility form; emits C only. |
| Package GC | Existing project cache collection command; invoke deliberately. |
| Ku：安装依赖 | Explicit `ku i` restore or source selection; supports source text and local file/directory selection when the chosen CLI supports it. |

When a package file is not its ku.mod main entry, Build asks for the explicit output path required by the CLI; the backend remains unchanged.

Build/IR/install tools tee output to the task terminal and Ku Output, with common credential patterns redacted in the latter. Stop a task/terminal to stop its own process tree. The extension does not call a Run action “debugging” and does not provide breakpoints.

The tested daily compiler rejects both `input()` and `input(prompt)` as undefined functions. No unsupported input snippet is advertised. Run still uses a terminal that accepts stdin for programs/toolchains which support it.

## Locked packages

Open a real Ku project with `ku.mod` and `ku.lock`, then run **Ku：安装依赖** deliberately to restore or add a dependency. The CLI owns manifest and lock updates. A reproducible local source such as `./vendor/pysubs2` works without registry/network access; Git/PyPI sources follow `ku help i` and may need additional CLI configuration.

After a package is installed, import completion lists actual public exports, for example `import { FromMs } from "@pysubs2/time"` when that package really exports `FromMs`. Navigation uses the current project's lock identity and readable Ku sources/generated binding declarations. It does not select another version from a neighbouring project. Missing/mismatched locks or caches explain the problem and suggest an explicit restore; hovering an import displays the detail. Binary-only packages have no fabricated source target.

Opening, saving, completing or navigating never installs packages, starts services, connects to a database or fetches from the network. Restricted Mode preserves static browsing but does not launch a workspace executable, check, build or install dependencies.

## Five-minute example

1. Select your installed CLI and save this as a `.ku` file, including in a Chinese/space-containing folder.
2. Type `reply.` after the `reply = Make()` line to inspect its `title` and `count` fields, then remove that temporary line. Hover `reply` and use Go to Definition on `name`.
3. Click Run, then Check and Build. Review the actual tool path/version in the status bar or Show Version.

```ku
struct Reply { title: str, count: int }

fn Make(): Reply {
    return Reply { title: "你好🙂", count: 2 }
}

fn main() {
    reply = Make()
    {title: name, count: number} = reply
    println(name)
    println(number)
}
```

Check, Run, the default native build and the generated program were all verified for this example; it prints `你好🙂` and `2`. The tested asynchronous HTTP/PG applications are listed in [VERIFICATION.md](VERIFICATION.md). The SDK native backend still rejects some other async/rest field projections and closure captures; editor type assistance does not expand that compiler subset.

## Development and verification

Requires Node.js 20+ and npm. From this directory:

```sh
npm ci --ignore-scripts
npm run lint
npm run typecheck
npm test
npm run build
npm run package
```

Tests reuse the existing extension model/formatter/diagnostic tests. A plugin-only CI matrix runs these commands on macOS, Linux and Windows without building Ku or running performance workloads. An unexecuted CI definition is not a platform acceptance result: actual runs and installed-VSIX checks are listed in [VERIFICATION.md](VERIFICATION.md). [CHANGELOG.md](CHANGELOG.md) records editor changes. Apache-2.0; original LICENSE.txt and NOTICE are included. This delivery does not publish to Marketplace or change Ku release tags.

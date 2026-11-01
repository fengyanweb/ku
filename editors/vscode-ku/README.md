# Ku Language for VS Code

[中文说明](README.zh-CN.md)

Editor support for Ku 0.0.19: `.ku`, `ku.mod`, and `ku.lock` highlighting, snippets, built-in completion/hover, local/import definition lookup, outline, conservative formatting, and commands backed by an installed Ku CLI. This is not an LSP and does not infer arbitrary receiver types or resolve package caches for navigation.

## Install

Install the delivered `ku-language-0.0.19.vsix` using **Extensions → … → Install from VSIX…**, then reload VS Code. For a deliberate reinstall of the same version:

```sh
code --install-extension /absolute/path/ku-language-0.0.19.vsix --force
```

The extension does not include or install Ku, database drivers, or native target packs. Install the CLI separately. This local delivery does not establish Marketplace or GitHub Release availability.

## Select Ku

`ku.executablePath` is a resource setting: each folder in a multi-root workspace can select its own CLI. Enter an executable path without shell quotes or arguments. Chinese characters and spaces are supported. For example:

```json
{
  "ku.executablePath": "${userHome}/tools/Ku SDK/ku"
}
```

`${workspaceFolder}`, `${userHome}`, `~/`, and paths relative to the containing workspace folder are supported. A relative path for a loose file is resolved against its directory. An invalid explicit setting is reported; it does not silently select another Ku. A probe must exit successfully and identify itself as `ku <version>`.

With the setting empty, search order is `ku` in the VS Code extension host's PATH, the containing workspace's `release/<rust-target>/ku[.exe]`, then `target/release` and `target/debug`. Bundle targets are Windows x64, Linux x64, and macOS ARM64; other architectures can use an explicit path or PATH. No SDK or PATH setting is modified. Remote windows use the CLI installed on the remote extension host. Restricted Mode and virtual workspaces do not activate the executable extension.

## Commands and diagnostics

| Command | Existing CLI invocation | Behavior |
| --- | --- | --- |
| Ku Run | `ku run FILE` | Requires a main function; runs in a process task terminal. |
| Check Current File | `ku check --json FILE` | Saves first, then shows diagnostics in Problems and details in Ku Output. |
| Show IR | `ku ir FILE` | Prints IR in a process task terminal. |
| Build Current File | `ku build FILE` | Uses the CLI's default backend/profile; its matching toolchain must be installed. |
| Emit Native C Source | `ku build --native FILE` | Emits C source only; this compatibility CLI form does not link an executable. |
| Package GC | `ku package gc FILE` | Uses the containing Ku project and removes unused dependency cache entries. |
| Show Version | `ku version` | Shows the selected CLI and extension versions. |

Commands save dirty files and stop when saving fails. Workspace tasks pass executable and arguments directly, without shell quoting or separator conversion, and can be stopped in VS Code's task UI. With no folder open, the CLI runs directly as the terminal executable with the same argument array; close that terminal to stop it. The CLI determines build output paths and toolchain errors.

Automatic checks run on open/save by default. The CLI reads saved files, so dirty buffers are never presented as freshly checked; editing invalidates affected diagnostics. `ku.checkOnChange` only debounces checks of saved documents, and does not add an unsaved-buffer compiler protocol. At most two check jobs run at once; newer checks replace pending work for that root and cancel obsolete checks. Checks keep the existing 15-second bound. Only an older CLI explicitly rejecting `--json` triggers text fallback; launch errors and timeouts remain visible in Ku Output.

Diagnostics use the compiler's actual target files, including imported sources, and map Unicode scalar columns to VS Code UTF-16 positions. Shared imported-file diagnostics are retained until all contributing roots clear them. Closing a root, changing Ku settings, and deactivation cancel its pending work. Source mapping has bounded file/byte/read limits; when unavailable, the diagnostic says so.

## Editor features and limits

Completion uses a built-in API/name model, including ordinary one-use async tasks, `std.http`, `std.task`, `std.json`, `std.bytes`, `std.net`, and time APIs. It is not semantic type inference. Local import completion preserves the directory prefix, including Chinese and space-containing paths; dependency prefix labels come from `ku.mod`. Package-cache navigation is not supported.

The formatter adjusts indentation, spacing, blank lines and adjacent `else`/`catch`/`finally`. It honors VS Code indentation options and file line endings, protects strings (including multiline literals) and comment contents, and returns unchanged input for unfinished literals/comments. It does not invoke a CLI formatter. Quick fixes and outline/definition lookup are heuristic; review edits as usual.

## Build locally

Requires Node.js 20+ and npm. From this directory:

```sh
npm ci --ignore-scripts
npm run lint
npm run typecheck
npm test
npm run build
npm run package -- --out /absolute/output/ku-language-0.0.19.vsix
```

`lint` uses TypeScript's strict unused-symbol checks; no separate lint dependency is required. Existing model/formatter/diagnostic tests are reused. The VSIX includes compiled `out/`, grammars, snippets, docs and notices, without development dependencies, source or temporary tests. For a development host, compile and launch `code --extensionDevelopmentPath=/absolute/path/to/editors/vscode-ku`.

See [CHANGELOG.md](CHANGELOG.md) and [VERIFICATION.md](VERIFICATION.md) for this local delivery. Apache-2.0; original `LICENSE.txt` and `NOTICE` are included.

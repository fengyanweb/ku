# Ku Language for VS Code

[完整中文说明](README.zh-CN.md) · [English](README.md#english)

## 中文

0.0.19 插件为 `.ku`、`ku.mod` 和 `ku.lock` 提供语法高亮、代码片段、补全、悬浮说明、跳转、格式化及运行命令。打开和保存文件时调用 `ku check`，将编译器诊断显示在实际出错的文件中。

- 支持 `async` / `await`、类型化箭头函数、对象解构，以及 `std.http`、`std.task`、`std.json`、`std.bytes`、`std.net` 等现有标准库的内置补全模型；`time.now()` 为 Unix 毫秒，`time.instant()` 为 Time 对象。
- 普通 async Task 是一次性句柄；插件不会把生命周期句柄的 `status`、`cancel`、`await_timeout` 方法套用到普通 Task。补全不是完整语言服务器，不能推断所有接收者类型或调用链。
- 插件与 CLI 分别安装。优先使用设置 `ku.executablePath`，随后查找 PATH 和工作区构建目录。状态栏读取插件包版本并与实际 CLI 比较，不使用旧版固定常量。
- 从匹配的 [GitHub Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) 安装包获取 `ku-language-0.0.19.vsix`，在 VS Code 的扩展菜单选择“从 VSIX 安装”。发布是否可下载以该 Release 为准，CI 验证夹具不是完整安装包。
- 插件不附带 CLI、数据库客户端库或 native target pack；`std.bytes` / `std.net` 需要对应包支持的 native C 执行路径。生产适用范围以该版发布说明和业务验收为准。

许可证为 Apache-2.0，原始 `LICENSE.txt` 和 `NOTICE` 随插件分发。下面提供英文功能说明及构建方法。

## English

This extension contributes syntax highlighting, snippets, diagnostics, commands, and editor intelligence for `.ku` files.

## Features

- Syntax highlighting for Ku 0.0.19 keywords including `async` / `await`, strings, template strings, numbers, built-in types, stdlib calls, and unsupported `let` / `mut` / `const`.
- `ku.mod` / `ku.lock` syntax highlighting.
- Language configuration for comments, brackets, auto-closing pairs, and surrounding pairs.
- Snippets for sync/async main, sync/async functions, `await task?`, generic functions, struct, enum, match, try/catch/finally, `std.fs`, `std.http`, `std.config`, `std.task`, bounded task stress, HTTP response usage, array map, string methods, and `array.try_get`.
- `ku check` diagnostics on open/save, shown in Problems with red squiggles.
- Command Palette commands: Run, Check, Show IR, Build, Build Native C, Package GC, Show Version.
- Right-click menu `Ku Run` in the editor and Explorer for `.ku` files; if the file does not define `fn main()`, the command shows a clear warning instead of disappearing.
- Editor title buttons for Run, Check, IR, and Build.
- Status bar version check for interpreter/plugin mismatch.
- Hover docs for async/await, Error, Result, std.fs, std.http, std.config, std.task, match, string and array helpers.
- Completion for keywords, base types, builtins, stdlib modules/functions, Error fields, HttpResponse fields, string/array methods, import paths, and package dependency prefixes. Member completion is context-aware: typing `http.se` offers `service` / `server`, and those constructors insert `service()` / `server()` so old property-style objects are not suggested; global completion no longer inserts bare `http.service` / `http.server`.
- Completion also covers `std.bytes`, `std.net`, `std.lexer`, `std.parser`, directory operations, and the `time.instant` / `time.elapsed` / `time.steady_millis` clocks. `time.now()` returns Unix milliseconds; `time.instant()` returns a Time object.
- The status-bar comparison version comes from `package.json` and is compared with the selected CLI's actual version.
- HTTP handler completions understand `req` / `app` / `router` names, so `req.` offers request fields instead of falling back to global stdlib functions. Normal route templates use `fn()` when request data is not needed and `fn(req)` when it is.
- Ordinary async task handles are one-use values returned by `async fn`; the extension does not suggest `status` / `cancel` / `await_timeout` lifecycle methods for them.
- Typed arrow functions are first-class values. Object indexing is strict by default; `object[key]?` is the explicit nullable lookup form. Object destructuring snippets cover rename/default/rest.
- JSON diagnostics are routed to the actual imported file, and stale checks cannot overwrite a newer save/change result.
- Go to definition for local functions/types, imported files, and exported imported symbols.
- Outline symbols for module, function, struct, enum, and local functions.
- Quick Fixes for missing `std.http` / `std.fs` / `std.config` / `std.task`, `let`, and `switch`.
- Built-in formatter for 4-space indentation, compressed blank lines, operator/comma spacing, and `} else` / `} catch` / `} finally` joining.

## Install From VSIX

Use `ku-language-0.0.19.vsix` from the matching target bundle in [v0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19), once that Release is available. Reinstall from VSIX or use `code --install-extension ./ku-language-0.0.19.vsix --force`. A CI `native_ci-*` artifact is a verification fixture, not the Ku CLI bundle. Consult the release notes and application-specific acceptance results for deployment limits.

To build the VSIX from the repository root instead:

```powershell
cd editors\vscode-ku
npm ci --ignore-scripts
npm test
npx --no-install vsce package --out ku-language-0.0.19.vsix
code --install-extension .\ku-language-0.0.19.vsix
```

Then reload VS Code from the Command Palette with `Developer: Reload Window`, or restart VS Code, and open any `.ku` file.

## Install Without Command Line

1. Open VS Code.
2. Open the Extensions view.
3. Click `...`.
4. Choose `Install from VSIX...`.
5. Pick `ku-language-0.0.19.vsix` from the extracted bundle or your local build.
6. Reload VS Code from the Command Palette with `Developer: Reload Window`, or restart VS Code.

## Install By Copying The Extension Folder

First run `npm ci --ignore-scripts` and `npm run compile` in this folder. If you do not want to use `vsce`, copy the compiled folder, including `out/`, `LICENSE.txt`, and `NOTICE`, into the VS Code extensions directory. Run this example from the repository root:

```powershell
$target = "$env:USERPROFILE\.vscode\extensions\ku-lang.ku-language-0.0.19"
New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -Path .\editors\vscode-ku\* -Destination $target -Recurse -Force
```

Then restart VS Code.

## Development Mode

Open `editors/vscode-ku` in VS Code and press `F5` to launch an Extension Development Host.

## Interpreter Path

The extension first tries `ku.executablePath` when configured, then `ku` from PATH, the matching workspace bundle under `release/<rust-target>/ku[.exe]`, and `target/release/ku[.exe]` / `target/debug/ku[.exe]`. Target bundles map Windows x64 to `x86_64-pc-windows-msvc`, Linux x64 to `x86_64-unknown-linux-gnu`, and Apple Silicon macOS to `aarch64-apple-darwin`; unsupported host architectures must use `ku.executablePath` or PATH.

If needed, set `ku.executablePath` in VS Code settings.

## Scope

Completion and hover text use the extension's built-in API model. They do not provide a full compiler language server or infer every receiver type and chained method. `ku check` supplies compiler diagnostics; commands still require an installed Ku executable. The extension does not install the CLI, database drivers, or native target packs. `std.bytes` / `std.net` need the native C execution path supported by the selected Ku package.

## License

Apache-2.0. The original `LICENSE.txt` and `NOTICE` are included in the extension package. Third-party components retain their own license notices.

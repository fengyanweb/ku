# Ku Language for VS Code

This extension contributes syntax highlighting, snippets, diagnostics, commands, and editor intelligence for `.ku` files.

## Features

- Syntax highlighting for Ku 0.0.13 keywords including `async` / `await`, strings, template strings, numbers, built-in types, stdlib calls, and unsupported `let` / `mut` / `const`.
- `ku.mod` / `ku.lock` syntax highlighting.
- Language configuration for comments, brackets, auto-closing pairs, and surrounding pairs.
- Snippets for sync/async main, sync/async functions, `await task?`, generic functions, struct, enum, match, try/catch/finally, `std.fs`, `std.http`, `std.config`, `std.task`, bounded task stress, HTTP response usage, array map, string methods, and `array.try_get`.
- `ku check` diagnostics on open/save, shown in Problems with red squiggles.
- Command Palette commands: Run, Check, Show IR, Build, Build Native C, Package GC, Show Version.
- Right-click menu `Ku Run` in the editor and Explorer for `.ku` files; if the file does not define `fn main()`, the command shows a clear warning instead of disappearing.
- Editor title buttons for Run, Check, IR, and Build.
- Status bar version check for interpreter/plugin mismatch.
- Hover docs for async/await, Error, Result, std.fs, std.http, std.config, std.task, match, string and array helpers.
- Completion for keywords, base types, builtins, stdlib modules/functions, Error fields, HttpResponse fields, string/array methods, import paths, and package dependency prefixes. Member completion is context-aware: typing `http.s` only offers `service` / `server` and inserts only the member name, so it will not become `http.http.server`.
- HTTP handler completions understand common `req` / `res` / `app` / `router` names, so `req.` offers request fields instead of falling back to global stdlib functions.
- Ordinary async task handles are one-use values returned by `async fn`; the extension does not suggest `status` / `cancel` / `await_timeout` lifecycle methods for them.
- Typed arrow functions are first-class values. Object indexing is strict by default; `object[key]?` is the explicit nullable lookup form.
- JSON diagnostics are routed to the actual imported file, and stale checks cannot overwrite a newer save/change result.
- Go to definition for local functions/types, imported files, and exported imported symbols.
- Outline symbols for module, function, struct, enum, and local functions.
- Quick Fixes for missing `std.http` / `std.fs` / `std.config` / `std.task`, `let`, and `switch`.
- Built-in formatter for 4-space indentation, compressed blank lines, operator/comma spacing, and `} else` / `} catch` / `} finally` joining.

## Install From VSIX

From the repository root:

```powershell
cd editors\vscode-ku
npx @vscode/vsce package --out ku-language-0.0.13.vsix
code --install-extension .\ku-language-0.0.13.vsix
```

Then reload VS Code from the Command Palette with `Developer: Reload Window`, or restart VS Code, and open any `.ku` file.

## Install Without Command Line

1. Open VS Code.
2. Open the Extensions view.
3. Click `...`.
4. Choose `Install from VSIX...`.
5. Pick `editors/vscode-ku/ku-language-0.0.13.vsix`.
6. Reload VS Code from the Command Palette with `Developer: Reload Window`, or restart VS Code.

## Install By Copying The Extension Folder

If you do not want to use `vsce`, copy this folder into the VS Code extensions directory:

```powershell
$target = "$env:USERPROFILE\.vscode\extensions\ku-lang.ku-language-0.0.13"
New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -LiteralPath .\editors\vscode-ku\* -Destination $target -Recurse -Force
```

Then restart VS Code.

## Development Mode

Open `editors/vscode-ku` in VS Code and press `F5` to launch an Extension Development Host.

## Interpreter Path

The extension uses `ku` from PATH first, then falls back to `release/ku.exe`, `target/release/ku.exe`, or `target/debug/ku.exe` in the workspace.

If needed, set `ku.executablePath` in VS Code settings.

# Ku Language for VS Code

This extension contributes syntax highlighting, snippets, diagnostics, commands, and editor intelligence for `.ku` files.

## Features

- Syntax highlighting for Ku 0.0.12 keywords, strings, template strings, numbers, built-in types, stdlib calls, and unsupported `let` / `mut` / `const`.
- `ku.mod` / `ku.lock` syntax highlighting.
- Language configuration for comments, brackets, auto-closing pairs, and surrounding pairs.
- Snippets for `main`, functions, generic functions, struct, enum, match, try/catch/finally, `std.fs`, `std.http`, HTTP response usage, array map, string methods, and `array.try_get`.
- `ku check` diagnostics on open/save, shown in Problems with red squiggles.
- Command Palette commands: Run, Check, Show IR, Build, Build Native C, Package GC, Show Version.
- Right-click menu `Ku Run` for `.ku` files that define `fn main()`.
- Editor title buttons for Run, Check, IR, and Build.
- Status bar version check for interpreter/plugin mismatch.
- Hover docs for Error, Result, std.fs, std.http, match, string and array helpers.
- Completion for keywords, base types, builtins, stdlib modules/functions, Error fields, HttpResponse fields, string/array methods, import paths, and package dependency prefixes.
- Go to definition for local functions/types, imported files, and exported imported symbols.
- Outline symbols for module, function, struct, enum, and local functions.
- Quick Fixes for missing `std.http` / `std.fs`, `let`, and `switch`.
- Built-in formatter for basic indentation.

## Install From VSIX

From the repository root:

```powershell
cd editors\vscode-ku
npx @vscode/vsce package
code --install-extension .\ku-language-0.0.12.vsix
```

Then reload VS Code from the Command Palette with `Developer: Reload Window`, or restart VS Code, and open any `.ku` file.

## Install Without Command Line

1. Open VS Code.
2. Open the Extensions view.
3. Click `...`.
4. Choose `Install from VSIX...`.
5. Pick `editors/vscode-ku/ku-language-0.0.12.vsix`.
6. Reload VS Code from the Command Palette with `Developer: Reload Window`, or restart VS Code.

## Install By Copying The Extension Folder

If you do not want to use `vsce`, copy this folder into the VS Code extensions directory:

```powershell
$target = "$env:USERPROFILE\.vscode\extensions\ku-lang.ku-language-0.0.12"
New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -LiteralPath .\editors\vscode-ku\* -Destination $target -Recurse -Force
```

Then restart VS Code.

## Development Mode

Open `editors/vscode-ku` in VS Code and press `F5` to launch an Extension Development Host.

## Interpreter Path

The extension auto-detects `release/ku.exe`, `target/release/ku.exe`, `target/debug/ku.exe`, or `ku` from PATH.

If needed, set `ku.executablePath` in VS Code settings.

# Ku Language for VS Code

This extension contributes syntax highlighting, language configuration, and snippets for `.ku` files.

It intentionally does not provide semantic diagnostics yet; use `ku check <file.ku>` for real language checking.

## Features

- Syntax highlighting for Ku 0.0.12 keywords, strings, template strings, numbers, built-in types, stdlib calls, deprecated `let` / `mut` / `switch`.
- Language configuration for comments, brackets, auto-closing pairs, and surrounding pairs.
- Snippets for `main`, functions, generic functions, struct, enum, match, try/catch/finally, `std.fs`, `std.http`, HTTP response usage, array map, string methods, and `array.try_get`.

## Install From VSIX

From the repository root:

```powershell
cd editors\vscode-ku
npx @vscode/vsce package
code --install-extension .\ku-language-0.0.12.vsix
```

Then reload VS Code and open any `.ku` file.

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

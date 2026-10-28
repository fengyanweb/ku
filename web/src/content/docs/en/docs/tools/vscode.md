---
title: VS Code
description: Install the Ku extension and configure running, checking, formatting and completion.
kuVersion: '0.0.19'
verifiedAt: '2026-10-21'
sources:
  [
    'editors/vscode-ku/package.json',
    'editors/vscode-ku/README.md',
    'editors/vscode-ku/src/language.ts',
    'editors/vscode-ku/src/completionModel.ts',
    '.github/scripts/export-release.py',
  ]
---

The Ku extension provides syntax highlighting, snippets, completion, formatting and compiler diagnostics for VS Code 1.85 or newer. Install the [Ku CLI](/en/docs/install/) as well; the extension invokes it to check and run programs.

## Install

Download your platform archive from the [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19), then extract it. Each archive contains `ku-language-0.0.19.vsix` alongside the CLI. The VSIX is inside the archive, rather than a separate Release asset; consult that release page for actual availability.

In VS Code, open **Extensions → … → Install from VSIX…**, select that file, then run **Developer: Reload Window**. You can also install from the extracted directory:

```sh
code --install-extension ./ku-language-0.0.19.vsix
```

This installation uses the released VSIX directly; it does not require a Marketplace listing.

## Select the Ku executable

Put `ku` on your `PATH`, or set **Ku: Executable Path** to its full path. For example, on macOS:

```json
{
  "ku.executablePath": "/Users/you/tools/ku/ku"
}
```

On Windows, select `ku.exe`. Keep the other files from the extracted archive in their original relative locations. Run **Ku: Show Version** to check the executable the extension finds. The status bar also compares the CLI and extension versions.

## Check and run a program

Save this as `hello.ku`:

```ku
fn main() {
    println("Hello, Ku!")
}
```

Use **Ku: Check Current File**, then **Ku Run**. Compiler diagnostics appear in **Problems**, and command output appears in the Ku output channel or terminal. Checking on open and save is enabled by default. Checking on every edit is optional through `ku.checkOnChange`.

| Command                | Purpose                                   |
| ---------------------- | ----------------------------------------- |
| Ku Run                 | Run the current `.ku` file with `main()`  |
| Ku: Check Current File | Check syntax and types                    |
| Ku: Show IR            | Inspect the intermediate representation   |
| Ku: Build Current File | Build with the CLI's default build path   |
| Ku: Build Native C     | Build with the native C backend           |
| Ku: Package GC         | Run the CLI package-cache cleanup command |
| Ku: Show Version       | Show the resolved CLI version             |

Format with **Format Document**, or save with `editor.formatOnSave` enabled. The extension also supports `ku.mod` / `ku.lock` highlighting, local and imported definitions, an outline, import completion and quick fixes for selected diagnostics.

## Completion and examples

Type `http.` for HTTP helpers, `time.` for date and clock operations, or `json.` for encoding and decoding. HTTP constructors are inserted as `http.service()` / `http.server()`. The `http.server` snippet creates a complete server; see the [HTTP reference](/en/docs/stdlib/http/) for its API and runnable examples.

Completion uses the extension's built-in API list and selected receiver names. It does not infer every variable type or offer a complete compiler language server. A completion item does not establish support in every backend; use the corresponding library's execution requirements.

## Update

Install the VSIX from the version of Ku you use, then reload VS Code. The public v0.0.19 archive contains the extension shipped with that release. Later repository changes to completion and hover text take effect only after the extension is rebuilt and installed; they do not update an already downloaded VSIX automatically.

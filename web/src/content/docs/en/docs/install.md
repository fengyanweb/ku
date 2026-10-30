---
title: Install and download
description: Download the Ku 0.0.19 experimental prerelease and understand platform and build requirements.
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-29'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Download **Ku 0.0.19** for your computer. Each experimental prerelease package includes the command-line tool, interpreter and VS Code extension.

## 1. Download Ku

| System  | Architecture          | Download                                                                                                                          | Size    |
| ------- | --------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ------- |
| macOS   | Apple Silicon / ARM64 | [Download macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB |
| Windows | x64                   | [Download Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB |
| Linux   | x64 / GNU             | [Download Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB |

## 2. Extract and run

Extract the **entire package** into an empty folder, such as `ku` in your home folder. Keep all extracted files together; do not move only the executable. Open a terminal in that folder.

macOS / Linux:

```sh
./ku -v
```

Windows PowerShell:

```powershell
.\ku.exe -v
```

You should see `ku 0.0.19`. You can now run and check `.ku` files; installing Rust is not required for these operations.

<details>
<summary>Optional: use ku from any folder</summary>

If you extracted to `~/ku`, add this line to your shell startup file, such as `~/.zshrc` or `~/.bashrc`:

```sh
export PATH="$HOME/ku:$PATH"
```

On Windows, open **Edit environment variables for your account**, edit the user `Path`, and add the full path of the extracted folder, for example `%USERPROFILE%\ku`. Keep the existing entries.

Open a new terminal and run `ku -v`.

</details>

## 3. Add the VS Code extension

In the VS Code Extensions view, choose **… → Install from VSIX…**, then select `ku-language-0.0.19.vsix` from the extracted folder and reload the window.

If Ku is not on `PATH`, set `ku.executablePath` to the full path of `ku` (`ku.exe` on Windows).

<details>
<summary>Verify the download</summary>

Compare the archive’s SHA-256 with [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS). The [release page](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) also provides a `.manifest.json` for each package with file checksums. The package includes further instructions in `README-INSTALL.md`.

</details>

<details>
<summary>Compiling your own Ku programs</summary>

`ku run`, `ku check` and `ku ./hello.ku` need the platform CLI only. The original public package includes a Runner runtime compiled by Rust **1.89.0**; default `ku build` additionally needs matching Rust and a system linker. Upgrading system Rust does not rebuild that runtime. Selecting another version may produce E0514; see [Runner troubleshooting](/en/docs/guide/errors/#runner-build).

`ku build --backend c --release hello.ku -o hello` is a separate C-native path requiring a target C compiler and linker. Use `hello.exe` on Windows. Native database programs also need matching database client libraries, which are not bundled.

</details>

## Updates and fix-candidate status

Extract an **entire new bundle** into a separate empty directory, verify it and run `ku -v` before switching `PATH`. Do not overlay a new CLI onto old runtime libraries. Keep the old directory for rollback and confirm the executable path in use. On Windows, close the relevant Ku processes before changing directories if files are in use.

The development branch is fixing Runner compiler selection and adding `ku doctor`. **These changes are not included in the original public 0.0.19 downloads above.** A candidate bundle verifies CLI/runtime/dependencies using `ku-runtime.json` and `ku-runtime-deps.json`, then selects an installed Rust compiler with matching complete identity. Only explicit `ku doctor --install-runner-toolchain` requests installation; it does not change the global default. See [candidate diagnostics](/en/docs/tools/cli/#candidate-diagnostics).

<details>
<summary>Uninstall</summary>

Preserve any source files and configuration you created, then remove the extracted folder, the `PATH` entry you added, and the VS Code extension.

</details>

[Next: run your first program](/en/docs/getting-started/)

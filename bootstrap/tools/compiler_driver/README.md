# Bootstrap compiler driver

Build the driver with a current stage-0 Ku and a C11 compiler.

```sh
python3 -B bootstrap/tools/compiler_driver/build.py \
    --ku "$(command -v ku)" --cc cc -o /tmp/ku-bootstrap
```

The builder stages the Ku modules privately, uses existing offline native C
emission and links the C host at O2. It publishes the requested output only
after linking succeeds and removes the staged source and generated C.
On POSIX, an interruption stops the active compiler process group before
staging is removed. Windows uses the repository's bounded Job Object process
owner during the build. Python and the build sources are only needed at build
time.

On Windows x64, use GCC-style `clang` with the MSVC tools and SDK available
in a Developer PowerShell, or select an x64 MinGW GCC executable with `--cc`.

```powershell
python -B bootstrap/tools/compiler_driver/build.py `
    --ku (Get-Command ku).Source --cc clang -o "$env:TEMP\ku-bootstrap.exe"
& "$env:TEMP\ku-bootstrap.exe" check-project .\modules.txt
```

Run it from the directory containing your input, or pass an absolute path.

```sh
cat > add.ku <<'KU'
fn AddOne(value: int): int { return value + 1 }
KU
/tmp/ku-bootstrap check ./add.ku
/tmp/ku-bootstrap emit ./add.ku AddOne > add.c
```

- `check` accepts one source module. It runs the bounded parser,
  declaration and explicit type-name passes, and implemented direct-call
  checks.
- `check-project modules.txt` reads the existing manifest format. Each line
  lists a normalized project-relative `.ku` file. The compiler reads these
  files relative to the manifest's directory, even when invoked elsewhere.
  It resolves relative imports, aliases, explicit types and supported direct
  call contracts across those modules. The manifest accepts up to 64 modules
  and 32,768 UTF-8 bytes. The path rules are lexical. Symlinks follow normal
  filesystem behavior.
- `emit` selects one supported integer function and writes its C11 source
  to standard output. The leading C comment names the generated symbol
  for a caller. Link that source with a C caller or harness.
- `emit-project modules.txt Function` selects a function from the first
  listed module. The emitted C includes its supported reachable callees
  from the project. It uses the same manifest and import resolution as
  `check-project`.
- `emit-program modules.txt` emits a complete C11 program, including its
  entry point. The first module defines `main(): null!`. The current
  program emitter supports integer `println` expressions, bound scalar
  callees, and the supported `return ok(null)` or static failure. Compile
  the result directly with `cc -std=c11 -O2 app.c -o app`.
- `emit-unit modules.txt Function` emits one supported C11 function and its
  external declarations. `emit-unit-plan modules.txt Function` emits the
  matching typed dependency plan. Select the source module by listing it
  first. Resolve the complete set of units with `bootstrap/tools/link_c.py`.
- Success returns zero. File, checker and emitter errors return one and
  report the existing diagnostic on standard error. Invalid command forms
  or arguments return two. Inputs must be nonempty UTF-8 and at most 512
  bytes per argument. The host expands relative input paths against the
  caller's directory within a 4096-byte buffer. Windows receives UTF-16
  arguments and validates the same limit after conversion. It accepts
  ordinary drive and UNC paths. Device and verbatim paths are rejected.

## Build a typed application

List the entry module first in `modules.txt` and include its imported project
modules. Define `fn main(): null!` in the entry module. Use the standalone
compiler and your target C compiler to build the reachable units and link
the program.

```sh
python3 -B bootstrap/tools/build_program.py \
    --compiler /tmp/ku-bootstrap --cc cc /path/to/project/modules.txt -o ./app
./app
```

The build uses the typed `emit-unit` path, including supported owned values,
Result propagation and file reads across module boundaries. The application
retains its launch directory for runtime input files. The supported language
subset is the same as `emit-unit`. Select the target compiler explicitly on
Windows and give the output a `.exe` suffix.


The private C entry checks the exact signature of its synchronous Ku
callee. This driver provides the command interface for the current bounded
self-hosted compiler.

The installed driver has been exercised on macOS ARM64, Linux GNU x64,
Windows MSVC x64, and Windows GNU x64. Use the corresponding host toolchain
to reproduce its platform results.

```sh
python3 -B bootstrap/tools/compiler_driver/test_driver.py \
    --ku "$(command -v ku)" --cc cc
```


## Verify the same-source compiler generations

Use a clean output directory and the currently installed Ku to build the seed
and check two subsequent generations from the same Git commit.

```sh
python3 -B bootstrap/tools/compiler_driver/selfhost/verify.py \
    --repo . --revision HEAD --ku "$(command -v ku)" --cc clang \
    --output-dir ./bootstrap-verification
```

The verifier uses the committed source bytes, records all three executable
identities, and compares the generated C and typed plans before reporting a
successful run. Its receipts also describe the strict O2 objects, binaries,
and available sanitizer checks. This workflow covers the current bounded
bootstrap compiler source and the linked application cases it supports.

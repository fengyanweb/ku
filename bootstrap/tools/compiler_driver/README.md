# Bootstrap compiler driver

Build the driver with a current stage-0 Ku and a POSIX C11 compiler.

```sh
python3 -B bootstrap/tools/compiler_driver/build.py \
    --ku "$(command -v ku)" --cc cc -o /tmp/ku-bootstrap
```

The builder stages the Ku modules privately, uses existing offline native C
emission and links the C host at O2. It publishes the requested output only
after linking succeeds and removes the staged source and generated C.
An interruption stops the active compiler process group before staging is
removed. Python is a build-time dependency. The executable runs after the
compiler source tree is removed.

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
- Success returns zero. File, checker and emitter errors return one and
  report the existing diagnostic on standard error. Invalid command forms
  or arguments return two. Inputs must be nonempty UTF-8 and at most 512
  bytes per argument. The host expands relative input paths against the
  caller's directory within a 4096-byte buffer.

The private C entry checks the exact signature of its synchronous Ku
callee. This driver provides the command interface for the current bounded
self-hosted compiler.

The native integration test has run on macOS ARM64. The builder targets
POSIX hosts. Windows is rejected until it has a verified UTF-16 argv adapter.

```sh
python3 -B bootstrap/tools/compiler_driver/test_driver.py \
    --ku "$(command -v ku)" --cc cc
```

# Bootstrap declaration names

Build the tool with the installed Ku compiler and place the source to inspect
in `input.ku` beside the resulting executable.

```sh
cd bootstrap/semantics
cp ../stage2/span.ku input.ku
ku build --release main.ku -o ku-names
./ku-names
```

Pass `-o ku-names.exe` on Windows and run `.\ku-names.exe` in PowerShell.

The tool builds a declaration table from the Stage 3 parser's bounded AST.
Functions, structs, enums and module declarations share one namespace. Rows
appear in source order and include their arena node ID and source span. The
pass checks reserved compiler names in declarations and signature bindings.
Errors include the source position.

Import entries remain in the validated graph. The pass reports their count and
retains their node IDs for later resolution. It does not open import files or
resolve exports, references, types or function bodies.

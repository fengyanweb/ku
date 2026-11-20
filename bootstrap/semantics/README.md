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

`type_names.ku` binds explicit type-name nodes to built-ins, local declarations
and named imports. Its result keeps both the original use node and the target
module and declaration node. The type syntax remains in the original AST for
subsequent layout and expression checks.

Import entries remain in the validated graph. The pass reports their count and
retains their node IDs for later resolution. It does not open import files or
resolve exports, references, types or function bodies.

`imports.ku` also exposes `BindSuppliedImports`. Callers pass previously
parsed modules under the exact relative paths used by named imports. For
example, supply the name table for `stage1/token.ku` under
`../stage1/token.ku` to bind the `Token` import of `stage2/span.ku`.
Each resulting binding identifies the requesting node and the exported
declaration's node in the supplied module. An absent module and a missing or
private export produce different positioned errors.

This pass takes explicit module inputs. It leaves glob, namespace, standard
library and package imports to a later stage.

## Emit C for a structure projection

The projection compiler checks field names, scalar field types and ownership
modes, then emits standalone C11. The following example compiles the real
`SpanFromToken` function from the Ku bootstrap sources. Use a copy of the
bootstrap tree so the tool's input files stay separate from the source checkout.

```sh
cd bootstrap/semantics
cp ../stage2/span.ku input.ku
cp ../stage1/token.ku supplier.ku
printf '../stage1/token.ku\n' > supplier_path.txt
printf 'SpanFromToken\n' > function.txt
ku build --release projection_main.ku -o ku-project
./ku-project > projection.c
cc -std=c11 -O2 -c projection.c -o projection.o
```

The generated function uses numeric declaration IDs in C identifiers. An
owned input struct releases its owned string fields after the scalar reads. A
View parameter retains the caller's storage. The current subset returns one
local struct of scalar projections. General function bodies and returning
owned fields remain outside this tool's supported scope.

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

## Check a set of relative modules

Build `project_main.ku` with the output next to the entry source, then copy the
executable into a trusted project. Put the project sources in their original
relative locations and list each path once in `modules.txt`, one path per line.
For the bootstrap context module the list can be:

```text
stage2/context.ku
stage2/span.ku
stage1/token.ku
```

Use `ku build --release project_main.ku -o ku-check-project` from this tool's
directory. With the resulting executable beside `modules.txt`, run it to print
the validated dependencies in dependency order and the imported declaration and
explicit type bindings. The tool reads the files named in the manifest and
resolves named relative imports against that set. This entry handles declaration
and explicit type names. Expression type checking and C generation are separate.

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

For a function with explicit `int` parameters and result, write `int` to
`mode.txt` and put the module in `input.ku`. Select a function such as `Twice`
from `examples/math.ku` in `function.txt`. This mode emits checked signed
64-bit arithmetic and direct, nonrecursive calls within the input module.

The compiler entry scans the module once and parses bounded declaration
windows. It retains the lexer, per-function, AST and work limits. The legacy
`ParseProgram` entry retains its 512-token whole-script limit.

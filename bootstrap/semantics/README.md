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
Sync functions, async functions, structs, enums and module declarations share
one namespace. Async declarations keep their own AST and name-table kind. Rows
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
resolves named relative imports against that set. It binds standard library
namespaces from `import { fs, string } from "std"`, `import "std.fs"` or
`import fs from "std.fs"`. A `STD` row gives the importing path, namespace,
import node and request node. These namespaces have no file dependency and
cannot be used as type names. The tool also checks argument counts for direct
calls to local and named-import functions, including import aliases. Names
that could be shadowed by a local binding and calls through a value are left
to lexical binding and expression type checking. A call-count error carries
the source path and the full call span. Once the arity matches, a scalar
literal passed directly to a parameter with an explicit scalar or scalar union
annotation must match one of its types. A mismatch points at the argument.
Arguments that need inference or contextual types remain deferred. Named async
functions use the same declaration and direct-call checks. The scalar C emitter
requires synchronous functions.
For explicit scalar result types and scalar unions, literal returns use the
same check. Control blocks share the surrounding function's result type.
Nested functions and function expressions use their own annotations.
Relative imports can omit the `.ku` suffix. Manifest entries still name the
normalized `.ku` files.

## Emit a complete scalar program

`EmitScalarProjectProgramC` binds and checks the supplied project modules.
The first module defines `fn main(): null!`. The entry can print integer
expressions with the builtin `println` and use nested `if` or `else`
branches. Every path must reach `return ok(null)` or an `Error` literal in
`fail`. Integer expressions use the existing checked scalar functions,
including relative imports with aliases. Conditions support boolean literals,
logical operators, and the scalar comparison subset.
An exported `null!` function can use the same statement forms with integer
parameters or borrowed strings.

```ku
import { Increment as Answer } from "./numbers.ku"

fn main(): null! {
    println(Answer(41))
    return ok(null)
}
```

The emitted C includes the executable's entry point and requires only C11
and its standard library. Successful completion exits with status 0. A
returned Error writes its message to stderr and exits with status 1.

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

The same mode accepts `str` and `&str` parameters on leaf functions returning
an `int` literal or parameter. It emits guarded returns of the form
`if (left == right) { return value }` followed by a final return. A guard can
compare string parameters and literals with `==` or `!=`. To try it, put the
complete `DigitValue` definition from `bootstrap/stage1/lexer.ku` in `input.ku`
and select `DigitValue` in `function.txt`. The emitted C compares UTF-8 bytes
and lengths. A by-value argument transfers its owned buffer to the function.
A `const KuScalarString*` argument borrows the caller's storage.

The same guarded form can return string literals. For example, copy the
complete `KeywordKind` definition from `bootstrap/stage1/lexer.ku` and select
it in `function.txt`. Its C result is a `KuBootstrapString` with static storage
and stays valid after the input is released. Read its bytes using `len` and
leave the storage unchanged.

The same mode can compile the complete `At` function from
`bootstrap/stage1/lexer.ku`. String composition supports `+` over literals,
bound `str` or `&str` parameters, and builtin `str` conversion of an int
parameter or literal. The compiler evaluates those pieces left to right
and writes them into one owned result. Read its bytes using `len` and free
`ptr` when `storage == 1`. Owned input strings are released before returning
and before a fatal allocation failure. Borrowed inputs stay with the caller.
Directly returning an input alias and arbitrary calls inside the composition
remain outside this subset.

An explicit `bool` result selects C11 `bool`. Guards and bool returns can
use bool literals, `!`, short-circuit `&&` and `||`, and string `==`,
`!=` or `.contains`. String operands are parameters or literals. The C code
searches by byte length, including embedded NUL, and treats an empty needle
as present. The emitter bounds boolean expression nesting at 32 levels.
Try the complete `IsDigit`, `IsIdentifierStart` and
`IsIdentifierContinue` definitions in `bootstrap/stage1/lexer.ku`.

The scalar mode also emits `null!` helpers such as `CheckTokenCapacity` and
`CheckStringCapacity` from the same file. Guards can compare signed integer
parameters and literals. The supported exits are builtin `return ok(null)`
and `fail` with exactly one string literal for each Error field, `domain`,
`code`, and `message`. The generated C uses the Stage0 `KuResult_null` layout.
Use `ok` to select success or error. A failed result's fields refer to static
UTF-8 bytes and require no release.

### Compile scalar calls across project modules

Use `mode.txt` with `project-int`, the entry function in `function.txt` and a
`modules.txt` manifest with the entry module first. Direct integer calls can
use named relative imports and aliases, for example `import { Add as Plus }
from "./math.ku"`. The selected function must meet the scalar rules above.
The generated C contains the reachable functions with checked arguments and
only exports the selected symbol. Missing modules, cyclic module dependencies,
and calls through a shadowed name are diagnosed before C is emitted.

### Construct a nested struct from a project module

Set `mode.txt` to `construct` and put the selected function name in
`function.txt`. List project-relative source paths in `modules.txt`, with
the module that defines the function first. Keep the source files in their
original relative locations. The tool resolves the selected module's
named imports using those paths.

From the repository root, build the real `DefaultParseContext` constructor
in a separate directory:

```sh
project=$(mktemp -d)
compiler=$(mktemp -d)
cp -R bootstrap/. "$compiler/"
mkdir "$project/stage1" "$project/stage2"
cp bootstrap/stage1/token.ku "$project/stage1/"
cp bootstrap/stage2/context.ku bootstrap/stage2/span.ku "$project/stage2/"
ku build --release "$compiler/semantics/projection_main.ku" -o "$compiler/semantics/ku-project"
mv "$compiler/semantics/ku-project" "$project/ku-project"
(
    cd "$project"
    printf 'construct\n' > mode.txt
    printf 'DefaultParseContext\n' > function.txt
    printf 'stage2/context.ku\nstage2/span.ku\nstage1/token.ku\n' > modules.txt
    ./ku-project > context.c
    cc -std=c11 -O2 -c context.c -o context.o
)
```

This mode accepts a function returning a named struct literal, optionally
after one Copy struct local initialized by a literal. Explicit by-value
parameters can initialize matching fields. An owned parameter transfers once;
unused owned parameters are released before returning. `MakeDiagnostic` uses
this path to transfer its five string buffers into the result. C11 uses
bounded nonrecursive layouts and static storage for string literals. The
constructor itself allocates no heap storage.

The same constructor can read matching `int` and `bool` fields from a bound
struct parameter, including a read-only View. `MakeExpressionNode`,
`MakeOperator` and `OperatorSpan` use this to keep absolute source coordinates.
Moving a parameter invalidates later reads, and a View stays with its caller.

For a function that reads struct fields and returns a diagnostic
string, use the same project files with `guarded` in `mode.txt` and
`ParseContextError` in `function.txt`. This mode supports int and bool locals,
checked addition and subtraction, short-circuit conditions and `if` branches.
It also emits `SpanCanonical`, using string literals, string fields and
explicit conversion of Copy integers to form an owned string. The C function
allocates the joined output once. An owned struct argument releases its
strings on return, allocation failure or arithmetic failure. A View argument
leaves its storage with the caller.

The same mode supports a `bool` result for predicates such as the original
`IsNamedStructStart`. It reads the four Token Views in source order and
preserves short-circuit conditions. The generated predicate does no allocation
or ownership transfer.

`HasFractionPoint`, called by the numeric lexer, uses `guarded` with a
`View [str]` parameter. This subset compares a cloned indexed string with
a literal. The array remains owned by the caller. The emitted predicate
preserves checked arithmetic, array bounds and the clone's failure point.
The typed link plan identifies this parameter as a readonly builtin array.

For the original `RelocateTokens` in `stage2/context.ku`, select `managed` and
keep the same project manifest. The emitter handles the loop, per-iteration
owners and array return. The no-relocation branch returns the original array
storage. Relocation produces independent token owners, and bounds, arithmetic
and allocation failures release all live storage before reporting the error.

`ScanIntegerPrefix` in `stage1/lexer.ku` uses `struct-result` to project the
complete decimal prefix loop. It borrows a `[str]`, checks each index, and
uses local owned strings and checked arithmetic within bounded control blocks.
Link its typed result plan with `project-int` providers for `IsDigit` and
`DigitValue` from the same manifest. The result takes ownership of the grown
lexeme buffer. `ScanNumber` still decides whether the same prefix belongs to an
integer or a decimal float before reporting an overflowing integer.

The `linked-str` and `struct-result` modes emit `DiagnosticCanonical` and
`ReadPrimaryAtom` from their original modules. Generated `extern` declarations
identify dependencies that must be linked as C functions with the bound ABI.
The struct Result owns one successful payload or one Error. Moving the result
clears the source record before cleanup.

`struct-result` also accepts an initial checked call to a declared same-module
record `Result`, followed by a consuming named-field pattern. Call arguments
use matching Copy values, literals, and existing Views. The pattern can rename
or ignore fields and takes selected `str` owners directly. The caller releases
unselected fields and propagates the original Error on failure. The typed
link plan records the provider and its nominal result. Patterns with defaults
or `...rest` and record Results retained for later use remain outside this subset.

Use `project-fields` to emit a bound field projection such as `SpanFromToken`
from the project manifest. `linked-null` emits an original `null!` function
such as `RejectAt`. It evaluates bound call arguments in source order and
transfers their owners to the declared C dependency. A returned Error owns
its domain, code and message.

`linked-str` also emits functions such as `TokenCanonical` that consume an
owned struct. A string field may move into the expression once. A named
function in the same source module is linked by its declaration path and
node, just like an imported function. The builtin `str` accepts checked
Copy integer fields in this expression subset. Concatenations and integer
conversions retain source evaluation order and release partially built
strings if allocation fails.

`Canonical` uses the consuming array form of `linked-str`. It takes one
declared struct array by value, formats each element with a bound function,
and appends a literal separator between elements. The loop transfers each
element once. If concatenation fails, cleanup releases the accumulator, the
current line and the remaining array elements. Its link signature names the
array separately from the nominal identity of its element type.

With the same manifest and selected function, append `-plan` to
`linked-str`, `linked-null`, `struct-result`, `construct`, `project-fields`,
`guarded` or `project-int` in `mode.txt` to emit a `ku-bootstrap-link-plan-v1`
JSON document on stdout. The `export` record describes the selected
function. `dependencies` lists the external project declarations used by
the emitted C, in emitter order. The `construct`, `project-fields`,
`guarded` and `project-int` plans have an empty `dependencies` list. Each record
contains its normalized `source_path`, declared AST `node`, source name,
C symbol, and the checked parameter and result signature. A struct type
uses its own declaration path and node. Builtin types use a null path and
node zero. The C type and passing mode describe the current emitted
carrier, including const pointer parameters and Result wrappers.

Use the path and node together to match an import to its source
declaration. Distinct modules can share the same function name and node
number. The generated C symbol can change when the manifest order changes.
Use the ordinary mode name to emit C for the same plan.

The compiler entry scans the module once and parses bounded declaration
windows. It retains the lexer, per-function, AST and work limits. The legacy
`ParseProgram` entry retains its 512-token whole-script limit.

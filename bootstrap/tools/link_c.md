# Link bootstrap C units

Generate each C file and its `ku-bootstrap-link-plan-v1` JSON using the same
module list and function selection. Pass both files for each unit. This command
links an entry with one provider.

```sh
python3 bootstrap/tools/link_c.py \
  --unit out/entry.plan.json out/entry.c \
  --unit out/provider.plan.json out/provider.c \
  --entry stage2/expression_support.ku 569 --caller out/main.c \
  -o out/compiler-test
```

`--entry` specifies the module path and declaration node in the entry plan.
`--caller` supplies a C `main` compiled in that unit. It can use the emitted
types and local function names directly. The linker includes it after the
entry C, so the caller file does not need to include the emitted unit itself.
If one of the supplied units already defines `main`, omit both options.

The linker checks the providers and all dependencies before starting the C
compiler. It compares the ordered parameter identities, passing modes, result
identity and Result representation. A struct identity comes from its module
path and declaration node. Equivalent types can use different C aliases in
separate units. Builtin carriers must agree, with aliases for the shared
bootstrap string layout recognized across emitters. Each provided unit
compiles once with C11 O2 and compile-time aliases for the checked symbols.
The C compiler checks the local declarations
against the plan. Only plan-described symbols receive aliases, so any other
externally visible helper names must be distinct across the units.

Use `--check-only` to print the validated aliases without compiling. `--cc`
selects the compiler executable. `--cflag` and `--ldflag` append an argument to
their respective commands. Use the `--cflag=VALUE` spelling when the value
begins with `-`.

For a provider without a generated plan, a `ku-bootstrap-link-receipt-v1`
can supply the same fields and a lowercase `c_sha256` for the exact C file.
Record the provider's own bound declaration and emitted ABI in the receipt.
The linker checks the digest and the receiving C declaration. The receipt
follows the same signature and dependency rules as a generated plan.

Run the local linker tests with this command.

```sh
python3 -B -m unittest discover -s bootstrap/tools -p test_link_c.py
```

Set `KU_LINK_CHAIN_UNITS` to the seven emitted parser-chain C and plan pairs
and `KU_LINK_CHAIN_REFERENCE` to the original Ku comparison JSON to also
run the `ReadPrimaryAtom` integration test. The pairs use the function name
with `.c.txt` and `.plan.json` extensions. Set `KU_LINK_CHAIN_SANITIZE=1` to
run that integration with AddressSanitizer and UndefinedBehaviorSanitizer.
